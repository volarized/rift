//! In-memory indexing and retrieval.

mod change_set;
mod chunk;
mod database;
mod dependency;
mod glob;
mod language;
mod lexical;
mod log;
mod semantic;

mod relationship;
mod revision;
mod vector;
mod workspace;

pub use change_set::{ChangeSet, FileDigest, PathChange, PathChanges, WorkspaceDigests};
pub use database::{DatabasePool, WorkspaceDatabase};
pub use dependency::{
    AnalyzedFile, DIRECTORY_DEPTH_MAX_DEFAULT, DependencyIndex, DependencyIndexLimits,
    DependencySymbolMatch, ManifestError, PackageAnalysis, PackageAnalyzer, PackageFiles,
    PackageIndex, PackageIndexError, PackageIndexFault, PackageIndexViolation, SkippedPackage,
    WALK_ENTRIES_MAX_DEFAULT, analyzer_manifest_path, analyzer_revision, package_files,
    render_analyzer_manifest,
};
pub use glob::{ForceIncludeReach, PathMatcher, PathVerdict};
pub use language::{EffectiveLanguage, WorkspaceLanguagePolicy};
pub use lexical::{
    LexicalChange, LexicalIndexError, LexicalIndexFault, LexicalIndexLimits, LexicalIndexViolation,
    LexicalMatch, LexicalRanking, LexicalSearchIndex, LexicalUnit, LexicalUnitKind, RevisionScoped,
};
pub use log::{
    LOG_BATCH_RECORDS_MAX, LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LOG_LEVELS,
    LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord, LogStore, StoredLogRecord,
};
pub use relationship::{
    RELATIONSHIP_EDGES_MAX, RelationshipEdge, RelationshipStore, produced_relationship_facets,
};
pub use revision::RevisionPaths;
pub use vector::{StoredVector, VectorStore};
pub use workspace::{
    IndexedFile, ReadableSymbol, SymbolMatch, SymbolMatchRank, TextSourceFile,
    WorkspaceFingerprint, WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexFault,
    WorkspaceIndexLimits, WorkspaceIndexViolation, WorkspaceIndexWarning, WorkspaceSourcePolicy,
    capture_digests, capture_digests_with_languages, source_line_matches, symbol_matches,
    text_line_matches,
};

/// Compile-time marker for index-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLayer;
