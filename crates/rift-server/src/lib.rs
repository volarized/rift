//! Long-lived workspace application service.

mod change;
mod configuration;
mod engine;
mod history;
mod hook;
mod patch;
mod read;
mod rename;
mod search;

pub use change::ChangeService;
pub use configuration::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, ConfigurationFault, load_configuration,
};
pub use engine::{EnginePool, EngineSlot};
pub use hook::{HookRun, HookStatus, run_hooks};
pub use read::{ReadError, ReadFault, ReadService};
pub use rename::{
    RENAME_FILE_BYTES_MAX, RENAME_FILE_EDITS_MAX, RENAME_FILES_MAX, RENAME_SWEEP_BYTES_MAX,
    RENAME_SWEEP_FILES_MAX, RENAME_SWEEP_FINDINGS_MAX, RenamePlan, RenameResolution, plan_rename,
};
pub use rift_core::CapturedStream;

/// Compile-time marker for server-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLayer;
