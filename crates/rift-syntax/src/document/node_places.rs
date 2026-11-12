//! The place of every syntax node inside the declaration or unit that holds it.

use std::collections::BTreeMap;

use super::{SyntaxDocument, SyntaxSymbol};

/// What holds one syntax node: a declaration, named by its position in
/// [`SyntaxDocument::symbols`], or the unit when no declaration's item subtree holds the node.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NodeHolder {
    /// The declaration at this position of the document's symbols.
    Declaration(usize),
    /// The source unit: the file's root and every node outside each declaration.
    Unit,
}

/// Where one syntax node sits: its holder, its pre-order index inside the holder, and the
/// index of its parent inside the same holder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodePlace {
    /// The declaration or unit that holds the node.
    pub holder: NodeHolder,
    /// Pre-order position inside the holder: 0 for a declaration's item node and for the
    /// unit's root, counting no node of a nested declaration's item subtree.
    pub index: usize,
    /// `index` of the node's parent inside the same holder. `None` for index 0.
    pub parent: Option<usize>,
}

impl SyntaxDocument {
    /// Places every node of [`Self::nodes`], in the same order.
    ///
    /// A declaration holds the item subtree rooted at its item node: the outermost node that
    /// spans its `item_range` and, when the provider names one, has its `node_kind`. A node
    /// belongs to the nearest declaration whose item node encloses it, so a nested
    /// declaration's subtree counts toward the nested declaration alone; a node no item node
    /// encloses belongs to the unit. `holds` selects the declarations that hold nodes at all,
    /// so a declaration without an established identity leaves its nodes to the enclosing
    /// holder. Two declarations naming one item node give it to the first.
    #[must_use]
    pub fn node_places(&self, holds: impl Fn(usize, &SyntaxSymbol) -> bool) -> Vec<NodePlace> {
        let nodes = self.nodes();
        let mut spans: BTreeMap<(u64, u64), Vec<usize>> = BTreeMap::new();
        for (position, node) in nodes.iter().enumerate() {
            spans
                .entry((node.range.start, node.range.end))
                .or_default()
                .push(position);
        }
        let mut items: BTreeMap<usize, usize> = BTreeMap::new();
        for (declaration, symbol) in self.symbols().iter().enumerate() {
            if !holds(declaration, symbol) {
                continue;
            }
            let item = spans
                .get(&(symbol.item_range.start, symbol.item_range.end))
                .and_then(|candidates| {
                    candidates.iter().copied().find(|candidate| {
                        symbol
                            .node_kind
                            .is_none_or(|kind| nodes[*candidate].kind == kind)
                    })
                });
            if let Some(item) = item {
                items.entry(item).or_insert(declaration);
            }
        }
        let mut counts: BTreeMap<NodeHolder, usize> = BTreeMap::new();
        let mut places: Vec<NodePlace> = Vec::with_capacity(nodes.len());
        for (position, node) in nodes.iter().enumerate() {
            let enclosing = node
                .parent
                .filter(|parent| *parent < position)
                .map(|parent| places[parent]);
            let holder = items.get(&position).map_or_else(
                || enclosing.map_or(NodeHolder::Unit, |parent| parent.holder),
                |declaration| NodeHolder::Declaration(*declaration),
            );
            let count = counts.entry(holder).or_default();
            let index = *count;
            *count += 1;
            let parent = enclosing
                .filter(|parent| index != 0 && parent.holder == holder)
                .map(|parent| parent.index);
            places.push(NodePlace {
                holder,
                index,
                parent,
            });
        }
        places
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeHolder, NodePlace};
    use crate::{SyntaxLimits, SyntaxSource, registry};

    const NESTED: &str = "/// Lamp.\npub mod lamp {\n    /// Light.\n    pub fn light() -> u8 {\n        1\n    }\n}\n\npub fn dim() {}\n";

    fn document(source: &str) -> crate::SyntaxDocument {
        let path = rift_core::ProjectPath::new("src/lib.rs").expect("path");
        registry::provider_for_extension("rs")
            .expect("the Rust provider claims rs")
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: source,
                },
                SyntaxLimits::default(),
            )
            .expect("the fixture parses")
    }

    fn holder_of(document: &crate::SyntaxDocument, name: &str) -> NodeHolder {
        let position = document
            .symbols()
            .iter()
            .position(|symbol| symbol.name == name)
            .expect("the fixture declares the name");
        NodeHolder::Declaration(position)
    }

    /// Each declaration numbers its own item subtree from its item node at 0, a nested
    /// declaration's subtree counts toward the nested declaration alone, and the unit numbers
    /// what no declaration holds from its root at 0.
    #[test]
    fn every_node_is_numbered_inside_the_declaration_or_unit_that_holds_it() {
        let document = document(NESTED);
        let places = document.node_places(|_, _| true);
        assert_eq!(places.len(), document.nodes().len());
        let (lamp, light, dim) = (
            holder_of(&document, "lamp"),
            holder_of(&document, "light"),
            holder_of(&document, "dim"),
        );
        for holder in [NodeHolder::Unit, lamp, light, dim] {
            let indexes: Vec<usize> = places
                .iter()
                .filter(|place| place.holder == holder)
                .map(|place| place.index)
                .collect();
            assert_eq!(
                indexes,
                (0..indexes.len()).collect::<Vec<_>>(),
                "{holder:?} counts its own nodes in pre-order from 0"
            );
        }
        let node_of = |kind: &str, text: &str| {
            let start = u64::try_from(NESTED.find(text).expect("fixture text")).expect("offset");
            document
                .nodes()
                .iter()
                .position(|node| node.kind == kind && node.range.start == start)
                .expect("the fixture holds the node")
        };
        assert_eq!(
            places[0],
            NodePlace {
                holder: NodeHolder::Unit,
                index: 0,
                parent: None
            }
        );
        let light_item = places[node_of("function_item", "pub fn light")];
        assert_eq!((light_item.holder, light_item.index), (light, 0));
        assert_eq!(
            light_item.parent, None,
            "an item node has no parent inside its holder"
        );
        let body = places[node_of("block", "{\n        1")];
        assert_eq!(body.holder, light);
        assert_eq!(
            places[node_of("integer_literal", "1\n")].parent,
            Some(body.index),
            "a parent is named by its index inside the same holder"
        );
        let doc = places[node_of("line_comment", "/// Light.")];
        assert_eq!(
            doc.holder, lamp,
            "documentation outside the nested item node stays with the enclosing declaration"
        );
        assert_eq!(
            places[node_of("line_comment", "/// Lamp.")].holder,
            NodeHolder::Unit
        );
    }

    /// A declaration that holds no nodes leaves its subtree to the enclosing holder.
    #[test]
    fn a_declaration_that_holds_no_nodes_leaves_them_to_the_enclosing_holder() {
        let document = document(NESTED);
        let light = holder_of(&document, "light");
        let places = document.node_places(|_, symbol| symbol.name != "light");
        assert!(places.iter().all(|place| place.holder != light));
        let lamp = holder_of(&document, "lamp");
        let lamp_count = places.iter().filter(|place| place.holder == lamp).count();
        let all = document.node_places(|_, _| true);
        assert!(
            lamp_count > all.iter().filter(|place| place.holder == lamp).count(),
            "the light subtree is numbered inside lamp once light holds nothing"
        );
    }
}
