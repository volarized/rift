//! In-memory indexing and retrieval.

mod change_set;
mod chunk;
mod glob;
mod lexical;
mod revision;
mod vector;
mod workspace;

pub use change_set::{ChangeSet, FileDigest, PathChange, PathChanges, WorkspaceDigests};
pub use glob::PathMatcher;
pub use lexical::{
    LexicalChange, LexicalIndexError, LexicalIndexFault, LexicalIndexLimits, LexicalIndexViolation,
    LexicalMatch, LexicalSearchIndex, LexicalUnit, LexicalUnitKind,
};
pub use vector::{SemanticVectorStore, StoredVector};
pub use workspace::{
    IndexedFile, SymbolMatch, SymbolMatchRank, TextSourceFile, WorkspaceFingerprint,
    WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexFault, WorkspaceIndexLimits,
    WorkspaceIndexViolation, WorkspaceSourcePolicy, capture_digests, source_line_matches,
    symbol_matches,
};

/// Compile-time marker for index-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLayer;
