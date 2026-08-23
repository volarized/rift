//! In-memory indexing and retrieval.

mod chunk;
mod glob;
mod lexical;
mod revision;
mod workspace;

pub use glob::PathMatcher;
pub use lexical::{
    LexicalIndexError, LexicalIndexFault, LexicalIndexLimits, LexicalIndexViolation, LexicalMatch,
    LexicalSearchIndex, LexicalUnit, LexicalUnitKind,
};
pub use workspace::{
    IndexedFile, SymbolMatch, SymbolMatchRank, TextSourceFile, WorkspaceFingerprint,
    WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexFault, WorkspaceIndexLimits,
    WorkspaceIndexViolation, WorkspaceSourcePolicy, source_line_matches, symbol_matches,
};

/// Compile-time marker for index-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLayer;
