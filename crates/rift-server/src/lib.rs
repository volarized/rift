//! Long-lived workspace application service.

mod configuration;
mod dependency;
mod embedded;
mod engine;
mod engine_read;
mod history;
mod map;
mod process;
mod read;
mod search;
mod traversal;

pub use configuration::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, ConfigurationFault, load_configuration,
};
pub use engine::{EnginePool, EngineSlot, LspProcessKey};
pub use engine_read::{EngineReferences, resolve_engine_references};
pub use read::{DependencyStore, ReadError, ReadFault, ReadService, wire_digest};
pub use rift_core::CapturedStream;

/// Compile-time marker for server-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLayer;
