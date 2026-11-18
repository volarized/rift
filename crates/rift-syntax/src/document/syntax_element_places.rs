//! The place of every syntax element inside the declaration or unit that holds it.

use std::collections::BTreeMap;

use super::{SyntaxDocument, SyntaxSymbol};

/// What holds one syntax element: a declaration, named by its position in
/// [`SyntaxDocument::symbols`], or the unit when no declaration's item subtree holds the syntax element.
/// The unit's file syntax element, which spans the whole unit outside the syntax element table, is syntax element 0 of the
/// unit.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SyntaxHolder {
    /// The declaration at this position of the document's symbols.
    Declaration(usize),
    /// The source unit: the file's root and every syntax element outside each declaration.
    Unit,
}

/// Where one syntax element sits: its holder, its pre-order index inside the holder, and the
/// index of its parent inside the same holder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxPlace {
    /// The declaration or unit that holds the syntax element.
    pub holder: SyntaxHolder,
    /// Pre-order position inside the holder: 0 for a declaration's item syntax element, 1 for the
    /// unit's root after its file syntax element at 0, counting no syntax element of a nested declaration's item
    /// subtree.
    pub index: usize,
    /// `index` of the syntax element's parent inside the same holder: the file syntax element for the unit's
    /// root, `None` for a declaration's item syntax element.
    pub parent: Option<usize>,
}

impl SyntaxDocument {
    /// Places every syntax element of [`Self::syntax elements`], in the same order.
    ///
    /// A declaration holds the item subtree rooted at its item syntax element: the outermost syntax element that
    /// spans its `item_range` and, when the provider names one, has its `syntax_element_kind`. A syntax element
    /// belongs to the nearest declaration whose item syntax element encloses it, so a nested
    /// declaration's subtree counts toward the nested declaration alone; a syntax element no item syntax element
    /// encloses belongs to the unit, whose syntax elements count from 1 after the file syntax element. `holds`
    /// selects the declarations that hold syntax elements at all, so a declaration without an
    /// established identity leaves its syntax elements to the enclosing holder. Two declarations naming one item syntax element give it to the first.
    #[must_use]
    pub fn syntax_element_places(
        &self,
        holds: impl Fn(usize, &SyntaxSymbol) -> bool,
    ) -> Vec<SyntaxPlace> {
        let syntax_elements = self.syntax_elements();
        let mut spans: BTreeMap<(u64, u64), Vec<usize>> = BTreeMap::new();
        for (position, syntax_element) in syntax_elements.iter().enumerate() {
            spans
                .entry((syntax_element.range.start, syntax_element.range.end))
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
                            .syntax_element_kind
                            .is_none_or(|kind| syntax_elements[*candidate].kind == kind)
                    })
                });
            if let Some(item) = item {
                items.entry(item).or_insert(declaration);
            }
        }
        let mut counts: BTreeMap<SyntaxHolder, usize> = BTreeMap::new();
        let mut places: Vec<SyntaxPlace> = Vec::with_capacity(syntax_elements.len());
        for (position, syntax_element) in syntax_elements.iter().enumerate() {
            let enclosing = syntax_element
                .parent
                .filter(|parent| *parent < position)
                .map(|parent| places[parent]);
            let holder = items.get(&position).map_or_else(
                || enclosing.map_or(SyntaxHolder::Unit, |parent| parent.holder),
                |declaration| SyntaxHolder::Declaration(*declaration),
            );
            let count = counts
                .entry(holder)
                .or_insert(usize::from(holder == SyntaxHolder::Unit));
            let index = *count;
            *count += 1;
            let parent = match enclosing {
                Some(parent) if parent.holder == holder && index != 0 => Some(parent.index),
                None if holder == SyntaxHolder::Unit => Some(0),
                _ => None,
            };
            places.push(SyntaxPlace {
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
    use super::{SyntaxHolder, SyntaxPlace};
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

    fn holder_of(document: &crate::SyntaxDocument, name: &str) -> SyntaxHolder {
        let position = document
            .symbols()
            .iter()
            .position(|symbol| symbol.name == name)
            .expect("the fixture declares the name");
        SyntaxHolder::Declaration(position)
    }

    /// Each declaration numbers its own item subtree from its item syntax element at 0, a nested
    /// declaration's subtree counts toward the nested declaration alone, and the unit numbers
    /// what no declaration holds from its root at 1, after its file syntax element at 0.
    #[test]
    fn every_syntax_element_is_numbered_inside_the_declaration_or_unit_that_holds_it() {
        let document = document(NESTED);
        let places = document.syntax_element_places(|_, _| true);
        assert_eq!(places.len(), document.syntax_elements().len());
        let (lamp, light, dim) = (
            holder_of(&document, "lamp"),
            holder_of(&document, "light"),
            holder_of(&document, "dim"),
        );
        for (holder, first) in [(SyntaxHolder::Unit, 1), (lamp, 0), (light, 0), (dim, 0)] {
            let indexes: Vec<usize> = places
                .iter()
                .filter(|place| place.holder == holder)
                .map(|place| place.index)
                .collect();
            assert_eq!(
                indexes,
                (first..first + indexes.len()).collect::<Vec<_>>(),
                "{holder:?} counts its own syntax elements in pre-order from {first}"
            );
        }
        let syntax_element_of = |kind: &str, text: &str| {
            let start = u64::try_from(NESTED.find(text).expect("fixture text")).expect("offset");
            document
                .syntax_elements()
                .iter()
                .position(|syntax_element| {
                    syntax_element.kind == kind && syntax_element.range.start == start
                })
                .expect("the fixture holds the syntax element")
        };
        assert_eq!(
            places[0],
            SyntaxPlace {
                holder: SyntaxHolder::Unit,
                index: 1,
                parent: Some(0)
            },
            "the root follows the file syntax element"
        );
        let light_item = places[syntax_element_of("function_item", "pub fn light")];
        assert_eq!((light_item.holder, light_item.index), (light, 0));
        assert_eq!(
            light_item.parent, None,
            "an item syntax element has no parent inside its holder"
        );
        let body = places[syntax_element_of("block", "{\n        1")];
        assert_eq!(body.holder, light);
        assert_eq!(
            places[syntax_element_of("integer_literal", "1\n")].parent,
            Some(body.index),
            "a parent is named by its index inside the same holder"
        );
        let doc = places[syntax_element_of("line_comment", "/// Light.")];
        assert_eq!(
            doc.holder, lamp,
            "documentation outside the nested item syntax element stays with the enclosing declaration"
        );
        assert_eq!(
            places[syntax_element_of("line_comment", "/// Lamp.")].holder,
            SyntaxHolder::Unit
        );
    }

    /// A declaration that holds no syntax elements leaves its subtree to the enclosing holder.
    #[test]
    fn a_declaration_that_holds_no_syntax_elements_leaves_them_to_the_enclosing_holder() {
        let document = document(NESTED);
        let light = holder_of(&document, "light");
        let places = document.syntax_element_places(|_, symbol| symbol.name != "light");
        assert!(places.iter().all(|place| place.holder != light));
        let lamp = holder_of(&document, "lamp");
        let lamp_count = places.iter().filter(|place| place.holder == lamp).count();
        let all = document.syntax_element_places(|_, _| true);
        assert!(
            lamp_count > all.iter().filter(|place| place.holder == lamp).count(),
            "the light subtree is numbered inside lamp once light holds nothing"
        );
    }
}
