//! Long-lived workspace application service.

mod change;
mod configuration;
mod hook;
mod patch;
mod read;
mod search;

pub use change::ChangeService;
pub use configuration::{ConfigurationError, ConfigurationFault, load_configuration};
pub use hook::{CapturedStream, HookRun, HookStatus, run_hooks};
pub use read::{ReadError, ReadFault, ReadService};

/// Compile-time marker for server-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLayer;
