//! In-memory indexing and retrieval.

mod workspace;

pub use workspace::{
    IndexedFile, SymbolMatch, SymbolMatchRank, WorkspaceIndex, WorkspaceIndexError,
    WorkspaceIndexLimits, WorkspaceIndexViolation,
};

/// Compile-time marker for index-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLayer;
