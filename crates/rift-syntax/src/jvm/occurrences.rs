//! Authored call, reference, and import positions for JVM collection.

use rift_error::{RiftError, errors};
use tree_sitter::Node;

use crate::extract::{byte_range, push_named_children};
use crate::{ByteRange, SyntaxLimits, SyntaxSource, parse};

use super::declarations::Rules;
use super::grammar::{Field, Keyword, Kinds, NodeKind};

/// Which authored positions to collect in a JVM source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JvmOccurrenceKind {
    /// Written callee names.
    Call,
    /// Written identifier uses.
    Reference,
    /// Written imported names.
    Import,
}

/// A source position and its enclosing declaration, when present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JvmOccurrence {
    /// Exact byte range of the authored name.
    pub range: ByteRange,
    /// Range of the closest enclosing declaration.
    pub enclosing: Option<ByteRange>,
}

/// Positions read from one source, with the completeness of the read.
#[derive(Debug)]
pub struct JvmOccurrences {
    /// Authored positions in source order.
    pub occurrences: Vec<JvmOccurrence>,
    /// Whether the collection finished without a parse error or position limit.
    pub complete: bool,
}

struct Visit<'tree> {
    node: Node<'tree>,
    parent: Option<Node<'tree>>,
    grandparent: Option<Node<'tree>>,
    depth: usize,
    enclosing: Option<ByteRange>,
    outside_references: bool,
}

impl<'tree> Visit<'tree> {
    fn root(node: Node<'tree>) -> Self {
        Self {
            node,
            parent: None,
            grandparent: None,
            depth: 0,
            enclosing: None,
            outside_references: false,
        }
    }

    fn child(&self, node: Node<'tree>) -> Self {
        Self {
            node,
            parent: Some(self.node),
            grandparent: self.parent,
            depth: self.depth + 1,
            enclosing: self.enclosing,
            outside_references: self.outside_references,
        }
    }
}

pub(super) fn collect(
    rules: &Rules,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    category: JvmOccurrenceKind,
    maximum: usize,
) -> Result<JvmOccurrences, RiftError> {
    let tree = parse::tree(source, limits, rules.kinds.grammar(), &[])?;
    let root = tree.root_node();
    let mut result = JvmOccurrences {
        occurrences: Vec::new(),
        complete: !root.has_error(),
    };
    let mut pending = vec![Visit::root(root)];
    let mut cursor = root.walk();
    let mut visited = 0;
    while let Some(mut visit) = pending.pop() {
        if visit.depth > limits.syntax_depth_max() {
            return errors::syntax::too_deep()
                .path(source.path)
                .syntax_depth_max(limits.syntax_depth_max())
                .fail();
        }
        let node = visit.node;
        visited += 1;
        if visited + pending.len() + node.named_child_count() > limits.syntax_elements_max() {
            return errors::syntax::too_many_syntax_elements()
                .path(source.path)
                .syntax_elements_max(limits.syntax_elements_max())
                .fail();
        }
        if node.is_missing() || node.is_extra() {
            continue;
        }
        let kinds = rules.kinds;
        let kind = kinds.kind(node);
        let declaration = rules.declaration_kind(node, visit.parent, visit.grandparent);
        let declaring = visit.parent.and_then(|parent| rules.name_node(parent)) == Some(node)
            || (kind == Some(NodeKind::Identifier) && declaration.is_some());
        visit.outside_references |=
            declaring || matches!(kind, Some(NodeKind::Package | NodeKind::Import));
        if matches!(kind, Some(NodeKind::Field | NodeKind::Property))
            || (!matches!(
                kind,
                Some(
                    NodeKind::InstanceOf
                        | NodeKind::PatternVariable
                        | NodeKind::LocalBinding
                        | NodeKind::Catch
                )
            ) && declaration.is_some()
                && rules.name_node(node).is_some())
        {
            visit.enclosing = Some(byte_range(node)?);
        }
        let range = match category {
            JvmOccurrenceKind::Call => call_name(kinds, node, kind, limits.syntax_depth_max()),
            JvmOccurrenceKind::Import if kind == Some(NodeKind::Import) => {
                import_name(kinds, node, limits.syntax_depth_max())
            }
            JvmOccurrenceKind::Reference
                if !visit.outside_references
                    && matches!(kind, Some(NodeKind::Identifier | NodeKind::TypeIdentifier)) =>
            {
                Some(node)
            }
            _ => None,
        };
        if let Some(range) = range {
            if result.occurrences.len() == maximum {
                result.complete = false;
                break;
            }
            result.occurrences.push(JvmOccurrence {
                range: byte_range(range)?,
                enclosing: visit.enclosing,
            });
        }
        push_named_children(&mut pending, &mut cursor, node, |child| visit.child(child));
    }
    result
        .occurrences
        .sort_by_key(|site| (site.range.start, site.range.end));
    result.occurrences.dedup_by_key(|site| site.range);
    Ok(result)
}

fn call_name<'tree>(
    kinds: &Kinds,
    node: Node<'tree>,
    kind: Option<NodeKind>,
    depth_max: usize,
) -> Option<Node<'tree>> {
    let callee = match kind? {
        NodeKind::Call => kinds.field(node, Field::Name).or_else(|| first_child(node)),
        NodeKind::ConstructorCall => kinds.field(node, Field::Type).or_else(|| first_child(node)),
        NodeKind::ConstructorDelegation => {
            return kinds
                .field(node, Field::Constructor)
                .or_else(|| kinds.token(node, Keyword::This))
                .or_else(|| kinds.token(node, Keyword::Super));
        }
        _ => None,
    }?;
    terminal_name(kinds, callee, depth_max)
}

fn import_name<'tree>(kinds: &Kinds, node: Node<'tree>, depth_max: usize) -> Option<Node<'tree>> {
    let name = kinds
        .child(node, NodeKind::QualifiedName)
        .or_else(|| kinds.child(node, NodeKind::Identifier))?;
    terminal_name(kinds, name, depth_max)
}

fn first_child(node: Node<'_>) -> Option<Node<'_>> {
    node.named_children(&mut node.walk())
        .find(|child| !child.is_extra() && !child.is_missing())
}

fn terminal_name<'tree>(
    kinds: &Kinds,
    mut node: Node<'tree>,
    depth_max: usize,
) -> Option<Node<'tree>> {
    for _ in 0..=depth_max {
        node = match kinds.kind(node)? {
            NodeKind::Identifier | NodeKind::TypeIdentifier => return Some(node),
            NodeKind::MemberAccess => kinds
                .field(node, Field::Member)
                .or_else(|| last_identifier(kinds, node)),
            NodeKind::QualifiedName => kinds
                .field(node, Field::Name)
                .or_else(|| last_identifier(kinds, node)),
            NodeKind::Type => last_identifier(kinds, node),
            NodeKind::GenericType => first_child(node),
            _ => None,
        }?;
    }
    None
}

fn last_identifier<'tree>(kinds: &Kinds, node: Node<'tree>) -> Option<Node<'tree>> {
    node.named_children(&mut node.walk())
        .filter(|child| {
            matches!(
                kinds.kind(*child),
                Some(NodeKind::Identifier | NodeKind::TypeIdentifier)
            ) && !child.is_missing()
        })
        .last()
}
