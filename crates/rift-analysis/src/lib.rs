//! Storage-independent source and semantic analysis for Rift.

#[cfg(feature = "collector")]
mod analyzer;
#[cfg(feature = "archive")]
pub mod archive;
mod chunk;
pub mod documentation;
mod enclosing;
#[cfg(feature = "collector")]
mod glob;
mod input;
#[cfg(feature = "collector")]
mod relationship;
mod revision;
#[cfg(feature = "collector")]
mod selection;
#[cfg(feature = "collector")]
mod semantic;
#[cfg(feature = "collector")]
mod source;

#[cfg(feature = "collector")]
pub use analyzer::{
    AnalyzedFile, PackageAnalysis, PackageAnalysisError, PackageAnalysisFault,
    PackageAnalysisViolation, PackageAnalyzer, PackageLanguage, StubForm, public_qualified_names,
};
pub use chunk::{TextChunk, text_chunks};
pub use enclosing::EnclosingDefinitions;
#[cfg(feature = "collector")]
pub use glob::{
    ForceIncludeReach, PathMatcher, PathVerdict, SourcePatternError, SourcePatternFault,
};
pub use input::{
    ExactPackageInput, ExactPackageLimits, PackageInputError, PackageInputFault,
    PackageInputViolation, PackageSource,
};
#[cfg(feature = "collector")]
pub use relationship::{RELATIONSHIP_EDGES_MAX, RelationshipEdge, RelationshipStore};
pub use revision::{
    ManifestError, analyzer_digest, analyzer_manifest_path, analyzer_revision,
    render_analyzer_manifest,
};
#[cfg(feature = "collector")]
pub use selection::{
    BUILD_OUTPUT_FOLDERS, CONTEXT7_FILE, Context7, Context7Error, Context7Fault, Context7Violation,
    DOCUMENTATION_EXCLUDED_FILES, DOCUMENTATION_EXCLUDED_FOLDERS, DocumentationSelection,
    PackageFileSelection, SelectedFiles, documentation_format,
};
#[cfg(feature = "collector")]
pub use semantic::{BuiltSemantics, PlacedDocument, WorkspaceSemanticError, WorkspaceSemantics};
#[cfg(feature = "collector")]
pub use source::{FileDigest, IndexedFile};
