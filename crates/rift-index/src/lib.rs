//! In-memory indexing and retrieval.

mod capture;
mod change_set;
mod chunk;
mod content_cache;
mod database;
mod database_thread;
mod documentation;
mod documentation_store;
mod language;
mod lexical;
mod log;
mod semantic;
mod trigram_store;

mod relationship;
mod revision;
mod vector;
mod workspace;

pub use capture::LastCapture;
pub use change_set::{
    ChangeSet, FileDigest, FileRecord, PathChange, PathChanges, WorkspaceDigests,
};
pub use content_cache::WorkspaceContentCache;
pub use database::{DatabasePool, HeldConnection, WorkspaceDatabase};
pub use language::{EffectiveLanguage, WorkspaceLanguagePolicy};
pub use lexical::{
    LexicalChange, LexicalIndexLimits, LexicalMatch, LexicalRanking, LexicalSearchIndex,
    LexicalStamp, PublishedIndex, RevisionScoped,
};
pub use log::{
    LOG_BATCH_RECORDS_MAX, LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LOG_LEVELS,
    LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord, LogStore, StoredLogRecord,
};
pub use relationship::{RELATIONSHIP_EDGES_MAX, RelationshipEdge, RelationshipStore};
pub use revision::RevisionPaths;
pub use rift_analysis::documentation::{
    DocumentationCollection, documentation_context, documentation_context_with_budget,
};
pub use rift_analysis::documentation::{
    DocumentationLayer, DocumentationProjection, DocumentationProjectionTarget,
};
pub use rift_analysis::{ForceIncludeReach, PathMatcher, PathVerdict};
pub use rift_error::RiftError;
pub use trigram_store::{PatternCandidate, PatternCandidates, TrigramBatch, UnindexedRows};
pub use vector::{StoredVector, VectorStore};
pub use workspace::{
    IndexRead, IndexedFile, IndexedFileNodes, ReadableSymbol, SymbolMatch, TextSourceFile,
    WorkspaceFingerprint, WorkspaceIndex, WorkspaceIndexLimits, WorkspaceIndexPreparation,
    WorkspaceIndexWarning, WorkspaceMapPaths, WorkspaceSourcePolicy, capture_digests,
    capture_digests_with_languages, capture_digests_with_languages_cancellable,
    capture_selected_paths_cancellable, capture_visible_digests_with_languages_cancellable,
    declaration_identity, relative_path, source_line_matches, symbol_matches, text_line_matches,
};

/// Compile-time marker for index-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLayer;
