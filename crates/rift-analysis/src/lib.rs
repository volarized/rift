//! Storage-independent source and semantic analysis for Rift.

#[cfg(feature = "collector")]
mod analyzer;
#[cfg(feature = "archive")]
pub mod archive;
mod chunk;
pub mod documentation;
mod input;
#[cfg(feature = "collector")]
mod relationship;
mod revision;
#[cfg(feature = "collector")]
mod semantic;
#[cfg(feature = "collector")]
mod source;

#[cfg(feature = "collector")]
pub use analyzer::{
    AnalyzedFile, PackageAnalysis, PackageAnalysisError, PackageAnalysisFault,
    PackageAnalysisViolation, PackageAnalyzer, PackageLanguage, documentation_format,
    public_qualified_names,
};
pub use chunk::{TextChunk, text_chunks};
pub use input::{
    ExactPackageInput, ExactPackageLimits, PackageInputError, PackageInputFault,
    PackageInputViolation, PackageSource,
};
#[cfg(feature = "collector")]
pub use relationship::{
    RELATIONSHIP_EDGES_MAX, RelationshipEdge, RelationshipStore, produced_relationship_facets,
};
pub use revision::analyzer_revision;
#[cfg(feature = "collector")]
pub use semantic::{BuiltSemantics, PlacedDocument, WorkspaceSemanticError, WorkspaceSemantics};
#[cfg(feature = "collector")]
pub use source::{FileDigest, IndexedFile};
