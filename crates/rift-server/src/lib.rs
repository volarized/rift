//! Long-lived workspace application service.

mod change;
mod configuration;
mod engine;
mod history;
mod hook;
mod patch;
mod read;
mod search;

pub use change::ChangeService;
pub use configuration::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, ConfigurationFault, load_configuration,
};
pub use engine::{EnginePool, EngineSlot, RESPAWN_PER_REQUEST_MAX};
pub use hook::{HookRun, HookStatus, run_hooks};
pub use read::{ReadError, ReadFault, ReadService};
pub use rift_core::CapturedStream;

/// Compile-time marker for server-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLayer;
