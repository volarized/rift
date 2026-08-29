//! Long-lived workspace application service.

mod change;
mod configuration;
mod diagnose;
mod engine;
mod history;
mod hook;
mod move_file;
mod patch;
mod publish;
mod read;
mod remove;
mod rename;
mod rewrite;
mod search;

pub use change::{ChangeService, HookSnapshot};
pub use configuration::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, ConfigurationFault, load_configuration,
};
pub use diagnose::{
    ENGINE_DIAGNOSTICS_PER_CHANGE_MAX, classified_engine_change_diagnostics,
    engine_change_set_diagnostics,
};
pub use engine::{EnginePool, EngineSlot, LspProcessKey};
pub use hook::{HookRun, HookStatus, hook_matches_paths, run_hook, run_hooks};
pub use move_file::{MovePlan, MoveResolution, plan_move};
pub use read::{ReadError, ReadFault, ReadService};
pub use remove::{
    REMOVE_REFERENCES_MAX, RemovePlan, RemoveResolution, plan_remove_node, plan_remove_symbol,
};
pub use rename::{
    RENAME_FILE_EDITS_MAX, RENAME_FILES_MAX, RENAME_SWEEP_BYTES_MAX, RENAME_SWEEP_FILES_MAX,
    RENAME_SWEEP_FINDINGS_MAX, RenamePlan, RenameResolution, plan_rename,
};
pub use rift_core::CapturedStream;

/// Compile-time marker for server-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLayer;
