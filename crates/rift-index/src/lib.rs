//! In-memory indexing and retrieval.

mod change_set;
mod chunk;
mod database;
mod documentation;
mod documentation_store;
mod language;
mod lexical;
mod log;
mod semantic;

mod relationship;
mod revision;
mod vector;
mod workspace;

pub use change_set::{ChangeSet, FileDigest, PathChange, PathChanges, WorkspaceDigests};
pub use database::{DatabasePool, HeldConnection, WorkspaceDatabase};
pub use language::{EffectiveLanguage, WorkspaceLanguagePolicy};
pub use lexical::{
    LexicalChange, LexicalIndexError, LexicalIndexFault, LexicalIndexLimits, LexicalIndexViolation,
    LexicalMatch, LexicalRanking, LexicalSearchIndex, LexicalStamp, PublishedIndex, RevisionScoped,
};
pub use log::{
    LOG_BATCH_RECORDS_MAX, LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LOG_LEVELS,
    LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord, LogStore, StoredLogRecord,
};
pub use relationship::{
    RELATIONSHIP_EDGES_MAX, RelationshipEdge, RelationshipStore, produced_relationship_facets,
};
pub use revision::RevisionPaths;
pub use rift_analysis::documentation::{
    DocumentationCollection, DocumentationError, documentation_context,
    documentation_context_with_budget,
};
pub use rift_analysis::documentation::{
    DocumentationLayer, DocumentationProjection, DocumentationProjectionTarget,
};
pub use rift_analysis::{
    ForceIncludeReach, PathMatcher, PathVerdict, SourcePatternError, SourcePatternFault,
};
pub use vector::{StoredVector, VectorStore};
pub use workspace::{
    IndexFailure, IndexedFile, ReadableSymbol, SymbolMatch, TextSourceFile, WorkspaceFingerprint,
    WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexFault, WorkspaceIndexLimits,
    WorkspaceIndexViolation, WorkspaceIndexWarning, WorkspaceSourcePolicy, capture_digests,
    capture_digests_with_languages, declaration_identity, source_line_matches, symbol_matches,
    text_line_matches,
};

/// Compile-time marker for index-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLayer;
