//! Source values shared by workspace and package analysis.

use std::sync::Arc;

pub use rift_core::FileDigest;
use rift_core::ProjectPath;
use rift_syntax::{SyntaxDocument, SyntaxFacts, SyntaxSymbol};

use crate::EnclosingDefinitions;

/// One immutable file enriched with syntax facts.
///
/// `Eq` is not derived: `syntax` carries [`SyntaxFacts`], which is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedFile {
    path: ProjectPath,
    source: Arc<String>,
    digest: FileDigest,
    executable: bool,
    syntax: Arc<SyntaxFacts>,
    /// Each declaration's complete span, keyed by its position in `syntax`'s symbols.
    declarations: EnclosingDefinitions<usize>,
}

impl IndexedFile {
    /// Constructs an indexed file from its captured source and syntax facts.
    #[must_use]
    pub fn new(
        path: ProjectPath,
        source: Arc<String>,
        digest: FileDigest,
        executable: bool,
        syntax: SyntaxDocument,
    ) -> Self {
        Self::new_with_shared_syntax(path, source, digest, executable, syntax.into_facts())
    }

    /// Constructs an indexed file from captured source and shared syntax facts.
    #[must_use]
    pub fn new_with_shared_syntax(
        path: ProjectPath,
        source: Arc<String>,
        digest: FileDigest,
        executable: bool,
        syntax: Arc<SyntaxFacts>,
    ) -> Self {
        let declarations = EnclosingDefinitions::new(
            syntax
                .symbols()
                .iter()
                .enumerate()
                .map(|(position, symbol)| (symbol.range.start, symbol.range.end, position)),
        );
        Self {
            path,
            source,
            digest,
            executable,
            syntax,
            declarations,
        }
    }

    /// Returns project-relative path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns complete UTF-8 source.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns shared source content for an owner that retains it.
    #[must_use]
    pub fn source_content(&self) -> &Arc<String> {
        &self.source
    }

    /// Returns the digest of the bytes this file was indexed from.
    #[must_use]
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }

    /// Whether this file was executable when the index captured it.
    #[must_use]
    pub const fn executable(&self) -> bool {
        self.executable
    }

    /// Records the executable bit captured from the source file's metadata.
    pub fn set_executable(&mut self, executable: bool) {
        self.executable = executable;
    }

    /// Returns captured syntax facts and their recorded complete named tree.
    #[must_use]
    pub fn syntax(&self) -> &SyntaxFacts {
        &self.syntax
    }

    /// Returns shared syntax facts for an owner that retains them.
    #[must_use]
    pub fn syntax_facts(&self) -> &Arc<SyntaxFacts> {
        &self.syntax
    }

    /// The smallest declaration whose complete span, attached documentation and
    /// attributes included, contains `start..end`, or `None` outside every declaration.
    ///
    /// The spans are sorted once when the file is indexed, so one lookup costs the
    /// nesting depth at `start` rather than the file's declaration count.
    #[must_use]
    pub fn enclosing_symbol(&self, start: u64, end: u64) -> Option<&SyntaxSymbol> {
        self.declarations
            .resolve(start, end)
            .and_then(|position| self.syntax.symbols().get(*position))
    }
}

impl get_size2::GetSize for IndexedFile {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: get_size2::GetSizeTracker>(
        &self,
        mut tracker: T,
    ) -> (usize, T) {
        let mut bytes: usize = 4 * std::mem::size_of::<usize>();
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.path, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.source, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.syntax, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.declarations, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        (bytes, tracker)
    }
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;
    use rift_syntax::{SyntaxLimits, SyntaxSource, registry};

    use super::*;

    #[test]
    fn indexed_file_keeps_complete_nodes_and_declaration_association() {
        let path = ProjectPath::new("src/lib.rs").expect("valid source path");
        let source = "pub fn beacon() {}\n";
        let document = registry::provider_for_extension("rs")
            .expect("Rust provider")
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: source,
                },
                SyntaxLimits::default(),
            )
            .expect("Rust source parses");
        let expected_nodes = document.nodes().to_vec();
        let symbol_range = document.symbols()[0].range;
        let expected_kind = document
            .nodes()
            .iter()
            .find(|node| node.range == symbol_range)
            .map(|node| node.kind);
        assert!(
            expected_kind.is_some(),
            "fixture declaration has syntax node"
        );

        let file = IndexedFile::new(
            path,
            Arc::new(source.to_owned()),
            FileDigest::of(source.as_bytes()),
            false,
            document,
        );

        assert_eq!(file.syntax().symbols()[0].node_kind, expected_kind);
        assert_eq!(file.syntax().nodes(), Some(expected_nodes.as_slice()));
    }
}
