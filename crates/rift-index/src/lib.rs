//! In-memory indexing and retrieval.

mod glob;
mod revision;
mod workspace;

pub use glob::PathMatcher;
pub use workspace::{
    IndexedFile, SymbolMatch, SymbolMatchRank, WorkspaceFingerprint, WorkspaceIndex,
    WorkspaceIndexError, WorkspaceIndexFault, WorkspaceIndexLimits, WorkspaceIndexViolation,
    WorkspaceSourcePolicy, source_line_matches, symbol_matches,
};

/// Compile-time marker for index-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLayer;
