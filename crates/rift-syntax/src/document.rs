//! Generic syntax facts one provider emits for one source file.

use rift_core::ProjectPath;
use rift_protocol::read::{Language, SymbolFacet};

/// Half-open UTF-8 byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteRange {
    /// First included byte.
    pub start: u64,
    /// First excluded byte.
    pub end: u64,
}

impl ByteRange {
    /// Reports whether byte position belongs to range.
    #[must_use]
    pub const fn contains(self, position: u64) -> bool {
        self.start <= position && position < self.end
    }
}

/// One named syntax-tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxNode {
    /// Grammar node kind, in the producing provider's vocabulary.
    pub kind: String,
    /// Node byte range.
    pub range: ByteRange,
    /// Parent index in the document node vector; `None` for the root.
    pub parent: Option<usize>,
    /// Whether the parser marked the node erroneous or missing.
    pub has_error: bool,
}

/// One named declaration extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxSymbol {
    /// Declared short name.
    pub name: String,
    /// Container-qualified name, unique within the file's symbol space.
    pub qualified_name: String,
    /// The containing symbol's qualified name; `None` for a declaration at
    /// the top level of the file.
    pub container: Option<String>,
    /// The provider's kind word, such as `function`; the wire kind composes
    /// it as `{language}.{kind}`.
    pub kind: &'static str,
    /// Portable categories this declaration falls into, in the provider's
    /// declared order.
    pub facets: Vec<SymbolFacet>,
    /// Authored visibility spelling, such as `pub(crate)`; `None` when the
    /// language states no visibility.
    pub visibility: Option<String>,
    /// Complete declaration byte range, extended over attached outer
    /// attributes and outer doc comments.
    pub range: ByteRange,
    /// The item node's own byte range, excluding any attached outer
    /// attributes and doc comments. Equal to `range` when nothing attaches.
    pub item_range: ByteRange,
    /// The implementation part: the grammar's body or value field; `None`
    /// for a declaration without one.
    pub body_range: Option<ByteRange>,
}

/// Immutable syntax facts for one source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxDocument {
    language: Language,
    path: ProjectPath,
    nodes: Vec<SyntaxNode>,
    symbols: Vec<SyntaxSymbol>,
    has_errors: bool,
}

impl SyntaxDocument {
    /// Assembles one document from a provider's extracted facts.
    pub(crate) const fn new(
        language: Language,
        path: ProjectPath,
        nodes: Vec<SyntaxNode>,
        symbols: Vec<SyntaxSymbol>,
        has_errors: bool,
    ) -> Self {
        Self {
            language,
            path,
            nodes,
            symbols,
            has_errors,
        }
    }

    /// Returns the language identity these facts are filed under.
    #[must_use]
    pub const fn language(&self) -> &Language {
        &self.language
    }

    /// Returns source path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns every named syntax node in pre-order.
    #[must_use]
    pub fn nodes(&self) -> &[SyntaxNode] {
        &self.nodes
    }

    /// Returns extracted declarations in source order.
    #[must_use]
    pub fn symbols(&self) -> &[SyntaxSymbol] {
        &self.symbols
    }

    /// Reports whether parser observed malformed syntax.
    #[must_use]
    pub const fn has_errors(&self) -> bool {
        self.has_errors
    }

    /// Returns nodes covering byte position, outermost first.
    #[must_use]
    pub fn nodes_at(&self, position: u64) -> Vec<&SyntaxNode> {
        self.nodes
            .iter()
            .filter(|node| node.range.contains(position))
            .collect()
    }
}
