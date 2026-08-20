//! Long-lived workspace application service.

mod change;
mod configuration;
mod read;

pub use change::ChangeService;
pub use configuration::{ConfigurationError, ConfigurationFault, load_configuration};
pub use read::{ReadError, ReadFault, ReadService};

/// Compile-time marker for server-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLayer;
