//! Long-lived workspace application service.

mod alternatives;
mod callee;
mod change;
mod configuration;
mod dependency;
mod embedded;
mod engine;
mod engine_read;
mod history;
mod history_fill;
mod map;
mod package_manager;
mod process;
mod query;
mod read;
mod recommendations;
mod search;
mod traversal;

pub use callee::{CalleeDeclaration, CalleePackage, CalleeRoots, PackageCallee};
pub use change::search_change;
pub use configuration::{
    CONFIGURATION_FILE_BYTES_MAX, configuration_file, load_configuration, profile_roots,
};
pub use engine::{EnginePool, EngineSlot, LspProcessKey};
pub use engine_read::{
    EngineQuery, EngineReferences, ProviderRequests, QueryPackageTarget, QuerySourceRequest,
    resolve_engine_references, resolve_query_references, uses_engine_references,
};
pub use history::{FillCounts, FillProgress, StoredHistory};
pub use history_fill::{
    AnalyzedCommit, FillPlan, HistoryAnalysis, PendingCommit, RELEASE_TAGS_MAX, UnversionedTag,
    release_version,
};
pub use read::{
    LocalSymbolViewInput, LocalSymbolViews, ReadService, ReadServiceBuild, accepted_limit,
    wire_digest,
};
pub use recommendations::{
    LspRecommendation, LspRecommendationBatch, LspRecommendationInputs, LspRecommendationStatus,
};
pub use rift_core::CapturedStream;
pub use rift_error::RiftError;
pub use rift_lsp::capabilities::PositionEncoding;
pub use search::{
    PatternBounds, SearchCandidateInput, SearchCandidateInputs, SearchCandidateItem, StoreAnswer,
    accepted_pattern, matched_fields, search_page_limit,
};

/// Compile-time marker for server-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLayer;
