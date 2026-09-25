export const exampleProjectFiles = [
  {
    path: "Cargo.toml",
    language: "toml",
    excerpt: false,
    code: `[package]
name = "example-project"
version = "0.1.0"
edition = "2024"

[dependencies]
tokio = { version = "^1.53.1", features = ["macros", "rt-multi-thread"] }
`,
  },
  {
    path: "README.md",
    language: "markdown",
    excerpt: false,
    code: `# Example project

A Rust project that reads workspace configuration from \`rift.toml\`.

- \`src/main.rs\` starts the application.
- \`src/config.rs\` loads the workspace configuration.
- \`src/error.rs\` defines the error type returned by the loader.

Rift reads the project structure, source, and documentation to help
an agent find the code relevant to its task.
`,
  },
  {
    path: "rift.toml",
    language: "toml",
    excerpt: false,
    code: `[source]
include = ["src/**", "Cargo.toml", "README.md", "rift.toml"]

[global]
enabled = true
`,
  },
  {
    path: "src/config.rs",
    language: "rust",
    excerpt: true,
    // Preserve the nodes example's UTF-8 byte offsets in this source excerpt.
    code: `use std::path::Path;

use crate::error::ConfigError;

/// Workspace configuration read from \`rift.toml\`.
pub struct Config {
    pub root: std::path::PathBuf,
}

/// Loads the workspace configuration from \`rift.toml\`.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path)?;
    parse_config(&text)
}
`,
  },
  {
    path: "src/error.rs",
    language: "rust",
    excerpt: false,
    code: `/// An error encountered while reading or parsing configuration.
pub type ConfigError = Box<dyn std::error::Error + Send + Sync>;
`,
  },
  {
    path: "src/main.rs",
    language: "rust",
    excerpt: false,
    code: `mod config;
mod error;

use std::path::Path;

use config::load_config;
use error::ConfigError;

#[tokio::main]
async fn main() -> Result<(), ConfigError> {
    let config = tokio::task::spawn_blocking(|| {
        load_config(Path::new("rift.toml"))
    })
    .await??;

    println!("Workspace root: {}", config.root.display());
    Ok(())
}
`,
  },
] as const;
