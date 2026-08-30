//! In-memory indexing and retrieval.

mod change_set;
mod chunk;
mod database;
mod glob;
mod language;
mod lexical;
mod log;
mod semantic;

mod revision;
mod vector;
mod workspace;

pub use change_set::{ChangeSet, FileDigest, PathChange, PathChanges, WorkspaceDigests};
pub use database::{DatabasePool, WorkspaceDatabase};
pub use glob::PathMatcher;
pub use language::{EffectiveLanguage, WorkspaceLanguagePolicy};
pub use lexical::{
    LexicalChange, LexicalIndexError, LexicalIndexFault, LexicalIndexLimits, LexicalIndexViolation,
    LexicalMatch, LexicalSearchIndex, LexicalUnit, LexicalUnitKind, RevisionScoped,
};
pub use log::{
    LOG_BATCH_RECORDS_MAX, LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LOG_MESSAGE_BYTES_MAX,
    LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord, LogStore, StoredLogRecord,
};
pub use vector::{SemanticVectorStore, StoredVector};
pub use workspace::{
    IndexedFile, ReadableSymbol, SymbolMatch, SymbolMatchRank, TextSourceFile,
    VisibleWorkspaceEntry, WorkspaceFingerprint, WorkspaceIndex, WorkspaceIndexError,
    WorkspaceIndexFault, WorkspaceIndexLimits, WorkspaceIndexViolation, WorkspaceIndexWarning,
    WorkspaceSourcePolicy, capture_digests, capture_digests_with_languages, source_line_matches,
    symbol_matches, text_line_matches,
};

/// Compile-time marker for index-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLayer;
