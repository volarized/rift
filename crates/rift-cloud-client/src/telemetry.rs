//! Opt-out settings for request counters. This module sends no requests and writes no files.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use rift_core::constants::WORKSPACE_CONFIGURATION_FILE;
use rift_protocol::configuration::{CONFIGURATION_FILE_BYTES_MAX, TelemetryConfiguration};
use serde::Deserialize;

/// User-level configuration below the home directory.
pub const USER_CONFIGURATION_FILE: &str = ".rift/config.toml";

/// Notice shown before counted archive requests.
pub const REQUEST_STATISTICS_NOTICE: &str = "Rift collects aggregate download and cloud request statistics by default to measure adoption and improve the service.\nYou can disable cloud and update statistics: rift config set --telemetry.disabled=true --user\nFor installation, set RIFT_TELEMETRY_DISABLED=true before running the installer.\nDetails: https://volar.sh/rift/docs/privacy";

/// Project, user, and environment opt-out settings for existing network requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Policy {
    root: PathBuf,
    home: Option<PathBuf>,
    environment_disabled: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Settings {
    telemetry: TelemetryConfiguration,
}

impl Policy {
    #[cfg(test)]
    pub(crate) fn for_test(root: &Path, home: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            home: Some(home.to_path_buf()),
            environment_disabled: false,
        }
    }
    /// Captures the project's root, home directory, and environment opt-out.
    #[must_use]
    pub fn of_process(root: &Path) -> Self {
        Self::of_environment(root, std::env::home_dir(), &|name| std::env::var_os(name))
    }

    fn of_environment(
        root: &Path,
        home: Option<PathBuf>,
        environment: &impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Self {
        let environment_disabled = environment("RIFT_TELEMETRY_DISABLED").is_some_and(|value| {
            value
                .to_str()
                .and_then(|value| value.parse().ok())
                .unwrap_or(true)
        });
        Self {
            root: root.to_path_buf(),
            home: home.filter(|path| !path.as_os_str().is_empty()),
            environment_disabled,
        }
    }

    /// Reads current opt-out files on the blocking pool before an existing request.
    /// Unresolved or invalid settings suppress counters without failing the request.
    ///
    /// # Cancel safety
    ///
    /// Cancellation discards the decision. File reads write no state.
    pub async fn disabled(&self) -> bool {
        let policy = self.clone();
        tokio::task::spawn_blocking(move || policy.read_disabled().unwrap_or(true))
            .await
            .unwrap_or(true)
    }

    fn read_disabled(&self) -> io::Result<bool> {
        if self.environment_disabled {
            return Ok(true);
        }
        let Some(home) = &self.home else {
            return Ok(true);
        };
        Ok(settings(&home.join(USER_CONFIGURATION_FILE))?.disabled
            || settings(&self.root.join(WORKSPACE_CONFIGURATION_FILE))?.disabled)
    }
}

fn settings(path: &Path) -> io::Result<TelemetryConfiguration> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(TelemetryConfiguration::default());
        }
        Err(error) => return Err(error),
    };
    let mut raw = String::new();
    file.take(CONFIGURATION_FILE_BYTES_MAX + 1)
        .read_to_string(&mut raw)?;
    if raw.len() as u64 > CONFIGURATION_FILE_BYTES_MAX {
        return Err(io::Error::other(
            "configuration exceeds the file byte limit",
        ));
    }
    let accepted: Settings = toml::from_str(&raw).map_err(io::Error::other)?;
    Ok(accepted.telemetry)
}

#[cfg(test)]
#[path = "telemetry/tests.rs"]
mod tests;
