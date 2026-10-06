//! The workspace's `[logs]` table, read before the process installs tracing.

use std::path::Path;

use rift_protocol::configuration::LogsConfiguration;

/// The workspace's `[logs]` table, or the default table while `rift.toml` is
/// absent or invalid.
///
/// The process reads this before it installs tracing, so the capture filter is
/// in force for the startup diagnostics too: a server that refuses to start is
/// one whose records a reader needs most.
#[must_use]
pub fn logs_configuration(root: &Path) -> LogsConfiguration {
    crate::validation::ConfigurationState::accept(root).logs_configuration()
}
