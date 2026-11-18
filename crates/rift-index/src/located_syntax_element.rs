//! A syntax element located in one held file, addressed by its place inside its symbol or unit.

use rift_core::SymbolId;
use rift_protocol::read::FILE_SYNTAX_ELEMENT_KIND;
use rift_syntax::{ByteRange, SyntaxDocument, SyntaxHolder, SyntaxMatch, SyntaxPlace};

/// Where one syntax element sits: inside a declaration of a symbol, or inside its unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyntaxAddress {
    /// Syntax element `index` of declaration `declaration` of `symbol`, counted in pre-order from the
    /// declaration's item syntax element at 0.
    Declaration {
        /// The symbol the declaration binds.
        symbol: SymbolId,
        /// Ordinal of the declaration among the symbol's declarations, in the order a symbol
        /// read lists them.
        declaration: usize,
        /// Pre-order position inside the declaration's item subtree.
        index: usize,
    },
    /// Syntax element `index` of its unit: 0 for the file syntax element, then the syntax elements no declaration holds.
    Unit {
        /// Pre-order position inside the unit.
        index: usize,
    },
}

/// One syntax element at a location: its address, its kind, and the bytes it spans.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocatedSyntax {
    /// The syntax element's place inside its symbol or unit.
    pub address: SyntaxAddress,
    /// Kind in the provider's vocabulary, or [`FILE_SYNTAX_ELEMENT_KIND`] for the whole unit.
    pub kind: &'static str,
    /// Byte range the syntax element spans in its file.
    pub range: ByteRange,
}

impl LocatedSyntax {
    /// The syntax element at `range` of `document`: the innermost syntax element spanning it exactly, or under
    /// [`SyntaxMatch::Enclosing`] the innermost one containing it, and the file syntax element of
    /// `source` when no grammar syntax element answers.
    ///
    /// `declarations` holds, for each of the document's declarations in its symbol order,
    /// the symbol it binds and its ordinal among that symbol's declarations; `None` for a
    /// declaration without an established identity, whose syntax elements its holder keeps. The
    /// work is linear in the document's syntax elements.
    #[must_use]
    pub fn in_document(
        document: &SyntaxDocument,
        source: &str,
        declarations: &[Option<(SymbolId, usize)>],
        range: ByteRange,
        matching: SyntaxMatch,
    ) -> Option<Self> {
        let places = document.syntax_element_places(|position, _| {
            declarations.get(position).is_some_and(Option::is_some)
        });
        let located = document
            .syntax_element_at(range, matching)
            .and_then(|syntax_element| {
                let position = document
                    .syntax_elements()
                    .iter()
                    .position(|candidate| std::ptr::eq(candidate, syntax_element))?;
                Some(Self {
                    address: SyntaxAddress::of(places[position], declarations),
                    kind: syntax_element.kind,
                    range: syntax_element.range,
                })
            });
        located.or_else(|| Self::file(source, range, matching))
    }

    /// The file syntax element of `source`, when it answers `range` under `matching`: it spans the
    /// whole unit, so it matches exactly that span or, enclosing, any range inside it. It is
    /// syntax element 0 of the unit.
    #[must_use]
    pub fn file(source: &str, range: ByteRange, matching: SyntaxMatch) -> Option<Self> {
        let whole = whole_range(source);
        let answers = match matching {
            SyntaxMatch::Exact => whole == range,
            SyntaxMatch::Enclosing => whole.encloses(range),
        };
        answers.then_some(Self {
            address: SyntaxAddress::Unit { index: 0 },
            kind: FILE_SYNTAX_ELEMENT_KIND,
            range: whole,
        })
    }
}

impl SyntaxAddress {
    /// The address `place` names, given the address of each declaration of its file.
    fn of(place: SyntaxPlace, declarations: &[Option<(SymbolId, usize)>]) -> Self {
        let held = match place.holder {
            SyntaxHolder::Declaration(position) => {
                declarations.get(position).and_then(Option::as_ref)
            }
            SyntaxHolder::Unit => None,
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
    use rift_protocol::read::FILE_SYNTAX_ELEMENT_KIND;
    use rift_syntax::{ByteRange, SyntaxMatch};

    use super::SyntaxAddress;
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
    fn syntax_element_at_resolves_exact_and_enclosing_ranges_to_their_addresses() {
        let (_root, index) = index();
        let path = ProjectPath::new("lib.rs").expect("path");
        let function = span("pub fn beacon() -> u8 {\n    7\n}");

        let exact = index
            .syntax_element_at(&path, function, SyntaxMatch::Exact)
            .expect("parse")
            .expect("the function syntax element");
        assert_eq!(exact.kind, "function_item");
        assert_eq!(exact.range, function);
        let SyntaxAddress::Declaration {
            symbol,
            declaration: 0,
            index: 0,
        } = &exact.address
        else {
            panic!("the function is syntax element 0 of its declaration: {exact:?}");
        };
        let seven = span("7");
        let enclosing = index
            .syntax_element_at(
                &path,
                ByteRange {
                    start: seven.start,
                    end: seven.start,
                },
                SyntaxMatch::Enclosing,
            )
            .expect("parse")
            .expect("the literal syntax element");
        assert_eq!(enclosing.kind, "integer_literal");
        assert!(
            matches!(
                &enclosing.address,
                SyntaxAddress::Declaration { symbol: held, declaration: 0, index }
                    if held == symbol && *index > 0
            ),
            "the literal lies inside the function's declaration: {enclosing:?}"
        );
        assert_eq!(
            index
                .syntax_element_at(&path, ByteRange { start: 1, end: 4 }, SyntaxMatch::Exact)
                .expect("parse"),
            None
        );
    }

    #[test]
    fn syntax_element_at_answers_the_file_syntax_element_where_no_grammar_syntax_element_spans_the_range()
     {
        // The grammar's root starts at the first token, after the leading blank lines.
        let (_root, index) = index();
        let path = ProjectPath::new("lib.rs").expect("path");
        let whole = crate::whole_range(SOURCE);

        let located = index
            .syntax_element_at(&path, whole, SyntaxMatch::Exact)
            .expect("parse")
            .expect("a syntax element spans the whole file");

        assert_eq!(located.range, whole);
        let past = ByteRange {
            start: 0,
            end: whole.end + 1,
        };
        assert_eq!(
            index
                .syntax_element_at(&path, past, SyntaxMatch::Enclosing)
                .expect("parse"),
            None
        );
        let missing = ProjectPath::new("absent.rs").expect("path");
        assert_eq!(
            index
                .syntax_element_at(&missing, whole, SyntaxMatch::Enclosing)
                .expect("no file"),
            None
        );
        assert_eq!(located.kind, FILE_SYNTAX_ELEMENT_KIND);
        assert_eq!(located.address, SyntaxAddress::Unit { index: 0 });
    }

    #[test]
    fn syntax_element_at_answers_the_file_syntax_element_of_a_text_file() {
        let (_root, index) = index();
        let path = ProjectPath::new("notes.txt").expect("path");
        let line = ByteRange { start: 4, end: 7 };

        let located = index
            .syntax_element_at(&path, line, SyntaxMatch::Enclosing)
            .expect("text read")
            .expect("the file syntax element");

        assert_eq!(located.kind, FILE_SYNTAX_ELEMENT_KIND);
        assert_eq!(located.range, ByteRange { start: 0, end: 8 });
        assert_eq!(located.address, SyntaxAddress::Unit { index: 0 });
        assert_eq!(
            index
                .syntax_element_at(&path, line, SyntaxMatch::Exact)
                .expect("text read"),
            None
        );
    }
}
