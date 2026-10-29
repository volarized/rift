//! A syntax node located in one held file, minted with its content identity.

use rift_protocol::read::{FILE_NODE_KIND, Language, NodeId};
use rift_syntax::{ByteRange, NodeMatch};

/// One node at a location: its content identity, its kind, and the bytes it spans.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocatedNode {
    /// Content identity over the node's language, kind, and exact bytes.
    pub id: NodeId,
    /// Kind in the provider's vocabulary, or [`FILE_NODE_KIND`] for the whole unit.
    pub kind: &'static str,
    /// Byte range the node spans in its file.
    pub range: ByteRange,
}

impl LocatedNode {
    /// Mints the located node of `kind` spanning `range` in `source`.
    ///
    /// # Panics
    ///
    /// Panics when `range` does not land inside `source` on character boundaries: every
    /// caller passes a range the same bytes' parse produced, or the whole source.
    #[must_use]
    pub fn of(language: &Language, kind: &'static str, range: ByteRange, source: &str) -> Self {
        let bytes = usize::try_from(range.start)
            .ok()
            .zip(usize::try_from(range.end).ok())
            .and_then(|(start, end)| source.as_bytes().get(start..end))
            .unwrap_or_else(|| {
                panic!(
                    "a node range lands inside the source its parse produced: range={range:?}, \
                     source_len={}",
                    source.len()
                )
            });
        Self {
            id: NodeId::for_content(language, kind, bytes),
            kind,
            range,
        }
    }

    /// The file node of `source`, when it answers `range` under `matching`: it spans the
    /// whole unit, so it matches exactly that span or, enclosing, any range inside it.
    #[must_use]
    pub fn file(
        language: &Language,
        source: &str,
        range: ByteRange,
        matching: NodeMatch,
    ) -> Option<Self> {
        let whole = whole_range(source);
        let answers = match matching {
            NodeMatch::Exact => whole == range,
            NodeMatch::Enclosing => whole.encloses(range),
        };
        answers.then(|| Self::of(language, FILE_NODE_KIND, whole, source))
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
    use rift_protocol::read::{FILE_NODE_KIND, Language, NodeId};
    use rift_syntax::{ByteRange, NodeMatch};

    use crate::{WorkspaceIndex, WorkspaceIndexLimits};

    const SOURCE: &str = "\n\npub fn beacon() -> u8 {\n    7\n}\n\n";

    fn index() -> (tempfile::TempDir, WorkspaceIndex) {
        let root = tempfile::tempdir().expect("workspace");
        fs::write(root.path().join("lib.rs"), SOURCE).expect("source");
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
    fn node_at_resolves_exact_and_enclosing_ranges_to_content_identities() {
        let (_root, index) = index();
        let path = ProjectPath::new("lib.rs").expect("path");
        let rust = Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        let function = span("pub fn beacon() -> u8 {\n    7\n}");

        let exact = index
            .node_at(&path, function, NodeMatch::Exact)
            .expect("parse")
            .expect("the function node");
        assert_eq!(exact.kind, "function_item");
        assert_eq!(exact.range, function);
        assert_eq!(
            exact.id,
            NodeId::for_content(&rust, "function_item", SOURCE[2..].trim_end().as_bytes())
        );
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
        assert_eq!(
            located.id,
            NodeId::for_content(&Language::text(), FILE_NODE_KIND, b"one\ntwo\n")
        );
        assert_eq!(
            index
                .node_at(&path, line, NodeMatch::Exact)
                .expect("text read"),
            None
        );
    }
}
