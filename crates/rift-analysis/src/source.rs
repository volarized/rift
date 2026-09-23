//! Source values shared by workspace and package analysis.

pub use rift_core::FileDigest;
use rift_core::ProjectPath;
use rift_syntax::SyntaxDocument;

/// One immutable file enriched with syntax facts.
///
/// `Eq` is not derived: `syntax` carries [`SyntaxDocument`], which is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedFile {
    path: ProjectPath,
    source: String,
    digest: FileDigest,
    executable: bool,
    syntax: SyntaxDocument,
}

impl IndexedFile {
    /// Constructs an indexed file from its captured source and syntax facts.
    #[must_use]
    pub fn new(
        path: ProjectPath,
        source: String,
        digest: FileDigest,
        executable: bool,
        syntax: SyntaxDocument,
    ) -> Self {
        Self {
            path,
            source,
            digest,
            executable,
            syntax,
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

    /// Returns syntax facts.
    #[must_use]
    pub const fn syntax(&self) -> &SyntaxDocument {
        &self.syntax
    }
}
