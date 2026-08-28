//! The rust-analyzer probe and the environment its fixtures launch under.

use std::collections::BTreeMap;
use std::path::Path;

use crate::engine_fixture::EngineFixture;

/// Rustup program that selects the pinned engine toolchain.
pub(crate) const RUSTUP_PROGRAM: &str = "rustup";
/// Rust toolchain carrying the live fixture engine.
pub(crate) const RUST_ANALYZER_TOOLCHAIN: &str = "1.98";
/// Engine command rustup runs inside the pinned toolchain.
pub(crate) const RUST_ANALYZER_PROGRAM: &str = "rust-analyzer";
/// Arguments that select rust-analyzer 1.98 without a directory override.
pub(crate) const RUST_ANALYZER_ARGUMENTS: [&str; 3] =
    ["run", RUST_ANALYZER_TOOLCHAIN, RUST_ANALYZER_PROGRAM];

/// Fails the test unless rust-analyzer 1.98 answers `--version` from the
/// fixture tree.
///
/// The probe invokes the same explicit rustup toolchain as the engine
/// child. A missing component fails here with the command's own words.
pub(crate) fn require_rust_analyzer(fixture_root: &Path) {
    let probe = std::process::Command::new(RUSTUP_PROGRAM)
        .args(RUST_ANALYZER_ARGUMENTS)
        .arg("--version")
        .current_dir(fixture_root)
        .output();
    match probe {
        Ok(output) if output.status.success() => {
            eprintln!(
                "rust-analyzer: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        Ok(output) => panic!(
            "`rustup run 1.98 rust-analyzer --version` failed in {}: install it with \
             `rustup toolchain install 1.98 --profile minimal --component rust-analyzer`. {}",
            fixture_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(error) => panic!(
            "`rustup` is not on PATH for {}: install rustup and then run \
             `rustup toolchain install 1.98 --profile minimal --component rust-analyzer`. {error}",
            fixture_root.display(),
        ),
    }
}

/// The fixture data the shared harness turns into a rust-analyzer 1.98 launch.
pub(crate) fn fixture() -> EngineFixture {
    EngineFixture {
        program: RUSTUP_PROGRAM,
        arguments: RUST_ANALYZER_ARGUMENTS
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        environment: BTreeMap::new(),
        initialization_options: None,
    }
}
