use std::path::Path;

use crate::error::ConfigError;

/// Workspace configuration read from `rift.toml`.
pub struct Config {
    pub root: std::path::PathBuf,
}

/// Loads the workspace configuration from `rift.toml`.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path)?;
    parse_config(path, &text)
}

/// Loads the configuration at the default path.
pub fn load_default_config() -> Result<Config, ConfigError> {
    load_config(Path::new("rift.toml"))
}

fn parse_config(path: &Path, text: &str) -> Result<Config, ConfigError> {
    if text.trim().is_empty() {
        return Err("empty workspace configuration".into());
    }
    let root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(Config { root: root.into() })
}
