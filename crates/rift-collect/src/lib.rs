//! Incremental collection over an admitted source inventory.

#[cfg(feature = "runtime")]
mod cache;
#[cfg(feature = "runtime")]
mod jvm;
#[cfg(feature = "runtime")]
mod lsp;
mod model;
#[cfg(feature = "runtime")]
mod stdio;

#[cfg(feature = "runtime")]
pub use cache::{CollectionCache, CollectionStats};
#[cfg(feature = "runtime")]
pub use jvm::JvmSyntaxCollector;
#[cfg(feature = "runtime")]
pub use lsp::LspCollector;
pub use model::{
    Batch, ByteRange, Category, CollectionError, CollectionInput, CollectionSource, Collector,
    Coverage, DEPENDENCIES_MAX, Evidence, Inventory, OBSERVATIONS_MAX, Observation, Producer,
    SOURCE_BYTES_MAX, SOURCE_COUNT_MAX, SourceAnchor, SourceDependencies,
};
#[cfg(feature = "runtime")]
pub use stdio::{StdioCollector, contract_schema};

/// Failure to admit, collect or retain a batch.
#[cfg(feature = "runtime")]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The source inventory or collection request exceeded a bound or was invalid.
    #[error("collection input: {0}")]
    Input(String),
    /// Source or batch admission failed.
    #[error(transparent)]
    Admission(#[from] CollectionError),
    /// A language engine refused an operation.
    #[error(transparent)]
    Engine(#[from] rift_error::RiftError),
    /// A producer returned an invalid or unsupported protocol message.
    #[error("collector protocol: {0}")]
    Protocol(String),
    /// A producer failed to finish within the request deadline.
    #[error("collector exceeded its request deadline")]
    Deadline,
    /// Operating system I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A message or retained batch could not be encoded or decoded.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A durable checkpoint could not be read or written.
    #[error(transparent)]
    Cache(#[from] toasty::Error),
}

#[cfg(feature = "runtime")]
const MESSAGE_BYTES_MAX: usize = 8 << 20;
#[cfg(feature = "runtime")]
const CONTRACT_VERSION: u32 = 1;
