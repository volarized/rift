//! Long-lived workspace application service.

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
mod process;
mod read;
mod search;
mod traversal;

pub use callee::{CalleeDeclaration, CalleePackage, CalleeRoots, PackageCallee};
pub use change::search_change;
pub use configuration::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, ConfigurationFault, load_configuration,
};
pub use engine::{EnginePool, EngineSlot, LspProcessKey};
pub use engine_read::{EngineReferences, resolve_engine_references, uses_engine_references};
pub use history::{FillCounts, FillProgress, StoredHistory};
pub use history_fill::{
    AnalyzedCommit, FillPlan, HistoryAnalysis, PendingCommit, RELEASE_TAGS_MAX, UnversionedTag,
    release_version,
};
pub use read::{ReadService, ReadServiceBuild, accepted_limit, wire_digest};
pub use rift_core::CapturedStream;
pub use rift_error::RiftError;
pub use rift_lsp::capabilities::PositionEncoding;
pub use search::{PatternBounds, StoreAnswer, accepted_pattern, search_page_limit};

/// Compile-time marker for server-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLayer;
