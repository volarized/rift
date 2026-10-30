//! A syntax node located in one held file, addressed by its place inside its symbol or unit.

use rift_core::SymbolId;
use rift_protocol::read::FILE_NODE_KIND;
use rift_syntax::{ByteRange, NodeHolder, NodeMatch, NodePlace, SyntaxDocument};

/// Where one node sits: inside a declaration of a symbol, or inside its unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeAddress {
    /// Node `index` of declaration `declaration` of `symbol`, counted in pre-order from the
    /// declaration's item node at 0.
    Declaration {
        /// The symbol the declaration binds.
        symbol: SymbolId,
        /// Ordinal of the declaration among the symbol's declarations, in the order a symbol
        /// read lists them.
        declaration: usize,
        /// Pre-order position inside the declaration's item subtree.
        index: usize,
    },
    /// Node `index` of its unit: 0 for the file node, then the nodes no declaration holds.
    Unit {
        /// Pre-order position inside the unit.
        index: usize,
    },
}

/// One node at a location: its address, its kind, and the bytes it spans.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocatedNode {
    /// The node's place inside its symbol or unit.
    pub address: NodeAddress,
    /// Kind in the provider's vocabulary, or [`FILE_NODE_KIND`] for the whole unit.
    pub kind: &'static str,
    /// Byte range the node spans in its file.
    pub range: ByteRange,
}

impl LocatedNode {
    /// The node at `range` of `document`: the innermost node spanning it exactly, or under
    /// [`NodeMatch::Enclosing`] the innermost one containing it, and the file node of
    /// `source` when no grammar node answers.
    ///
    /// `declarations` holds, for each of the document's declarations in its symbol order,
    /// the symbol it binds and its ordinal among that symbol's declarations; `None` for a
    /// declaration without an established identity, whose nodes its holder keeps. The
    /// work is linear in the document's nodes.
    #[must_use]
    pub fn in_document(
        document: &SyntaxDocument,
        source: &str,
        declarations: &[Option<(SymbolId, usize)>],
        range: ByteRange,
        matching: NodeMatch,
    ) -> Option<Self> {
        let places = document
            .node_places(|position, _| declarations.get(position).is_some_and(Option::is_some));
        let located = document.node_at(range, matching).and_then(|node| {
            let position = document
                .nodes()
                .iter()
                .position(|candidate| std::ptr::eq(candidate, node))?;
            Some(Self {
                address: NodeAddress::of(places[position], declarations),
                kind: node.kind,
                range: node.range,
            })
        });
        located.or_else(|| Self::file(source, range, matching))
    }

    /// The file node of `source`, when it answers `range` under `matching`: it spans the
    /// whole unit, so it matches exactly that span or, enclosing, any range inside it. It is
    /// node 0 of the unit.
    #[must_use]
    pub fn file(source: &str, range: ByteRange, matching: NodeMatch) -> Option<Self> {
        let whole = whole_range(source);
        let answers = match matching {
            NodeMatch::Exact => whole == range,
            NodeMatch::Enclosing => whole.encloses(range),
        };
        answers.then_some(Self {
            address: NodeAddress::Unit { index: 0 },
            kind: FILE_NODE_KIND,
            range: whole,
        })
    }
}

impl NodeAddress {
    /// The address `place` names, given the address of each declaration of its file.
    fn of(place: NodePlace, declarations: &[Option<(SymbolId, usize)>]) -> Self {
        let held = match place.holder {
            NodeHolder::Declaration(position) => {
                declarations.get(position).and_then(Option::as_ref)
            }
            NodeHolder::Unit => None,
        };
        match held {
            Some((symbol, declaration)) => Self::Declaration {
                symbol: symbol.clone(),
                declaration: *declaration,
                index: place.index,
            },
            None => Self::Unit { index: place.index },
        }
    }
}

/// The byte range spanning all of `source`.
#[must_use]
pub fn whole_range(source: &str) -> ByteRange {
    ByteRange {
        start: 0,
        end: u64::try_from(source.len()).unwrap_or(u64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rift_core::{ProjectPath, SourceVisibility, TextFileInclusion};
    use rift_protocol::read::FILE_NODE_KIND;
    use rift_syntax::{ByteRange, NodeMatch};

    use super::NodeAddress;
    use crate::{WorkspaceIndex, WorkspaceIndexLimits};

    const SOURCE: &str = "\n\npub fn beacon() -> u8 {\n    7\n}\n\n";

    fn index() -> (tempfile::TempDir, WorkspaceIndex) {
        let root = tempfile::tempdir().expect("workspace");
        fs::write(root.path().join("lib.rs"), SOURCE).expect("source");
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"beacon\"\nversion = \"1.0.0\"\n[lib]\npath = \"lib.rs\"\n",
        )
        .expect("manifest");
        fs::write(root.path().join("notes.txt"), "one\ntwo\n").expect("text");
        let index = WorkspaceIndex::build(
            root.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("index");
        (root, index)
    }

    fn span(text: &str) -> ByteRange {
        let start = SOURCE.find(text).expect("fixture text") as u64;
        ByteRange {
            start,
            end: start + text.len() as u64,
        }
    }

    #[test]
    fn node_at_resolves_exact_and_enclosing_ranges_to_their_addresses() {
        let (_root, index) = index();
        let path = ProjectPath::new("lib.rs").expect("path");
        let function = span("pub fn beacon() -> u8 {\n    7\n}");

        let exact = index
            .node_at(&path, function, NodeMatch::Exact)
            .expect("parse")
            .expect("the function node");
        assert_eq!(exact.kind, "function_item");
        assert_eq!(exact.range, function);
        let NodeAddress::Declaration {
            symbol,
            declaration: 0,
            index: 0,
        } = &exact.address
        else {
            panic!("the function is node 0 of its declaration: {exact:?}");
        };
        let seven = span("7");
        let enclosing = index
            .node_at(
                &path,
                ByteRange {
                    start: seven.start,
                    end: seven.start,
                },
                NodeMatch::Enclosing,
            )
            .expect("parse")
            .expect("the literal node");
        assert_eq!(enclosing.kind, "integer_literal");
        assert!(
            matches!(
                &enclosing.address,
                NodeAddress::Declaration { symbol: held, declaration: 0, index }
                    if held == symbol && *index > 0
            ),
            "the literal lies inside the function's declaration: {enclosing:?}"
        );
        assert_eq!(
            index
                .node_at(&path, ByteRange { start: 1, end: 4 }, NodeMatch::Exact)
                .expect("parse"),
            None
        );
    }

    #[test]
    fn node_at_answers_the_file_node_where_no_grammar_node_spans_the_range() {
        // The grammar's root starts at the first token, after the leading blank lines.
        let (_root, index) = index();
        let path = ProjectPath::new("lib.rs").expect("path");
        let whole = crate::whole_range(SOURCE);

        let located = index
            .node_at(&path, whole, NodeMatch::Exact)
            .expect("parse")
            .expect("a node spans the whole file");

        assert_eq!(located.range, whole);
        let past = ByteRange {
            start: 0,
            end: whole.end + 1,
        };
        assert_eq!(
            index
                .node_at(&path, past, NodeMatch::Enclosing)
                .expect("parse"),
            None
        );
        let missing = ProjectPath::new("absent.rs").expect("path");
        assert_eq!(
            index
                .node_at(&missing, whole, NodeMatch::Enclosing)
                .expect("no file"),
            None
        );
        assert_eq!(located.kind, FILE_NODE_KIND);
        assert_eq!(located.address, NodeAddress::Unit { index: 0 });
    }

    #[test]
    fn node_at_answers_the_file_node_of_a_text_file() {
        let (_root, index) = index();
        let path = ProjectPath::new("notes.txt").expect("path");
        let line = ByteRange { start: 4, end: 7 };

        let located = index
            .node_at(&path, line, NodeMatch::Enclosing)
            .expect("text read")
            .expect("the file node");

        assert_eq!(located.kind, FILE_NODE_KIND);
        assert_eq!(located.range, ByteRange { start: 0, end: 8 });
        assert_eq!(located.address, NodeAddress::Unit { index: 0 });
        assert_eq!(
            index
                .node_at(&path, line, NodeMatch::Exact)
                .expect("text read"),
            None
        );
    }
}
