//! The rust-analyzer probe and the real `[engines.rust]` table, for the
//! end-to-end lane driven through the real `rift` binary.
//!
//! Mirrors `rift-mcp`'s own `rust_engine.rs`; Cargo compiles each crate's
//! integration tests as their own binary, so the module is duplicated
//! rather than shared.

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

/// The real `[engines.rust]` table, built from [`fixture`].
pub(crate) fn rust_engine_configuration() -> String {
    fixture().configuration_toml()
}

/// The fixture data the shared harness turns into the `[engines.rust]`
/// table: rust-analyzer 1.98 over the fixture's cargo project.
///
/// Coverage starts several cold rust-analyzer processes in parallel. Its
/// fixture retry table keeps shipped pacing but extends attempts to 16, a
/// 25.75s wait bound, so instrumented process contention cannot turn this
/// engine test into a machine-speed test.
pub(crate) fn fixture() -> EngineFixture {
    EngineFixture {
        name: "rust",
        program: RUSTUP_PROGRAM,
        arguments: RUST_ANALYZER_ARGUMENTS.to_vec(),
        languages: vec!["rust"],
        extra_toml: "\n[engines.rust.retry]\nattempts = 16\n".to_owned(),
    }
}
