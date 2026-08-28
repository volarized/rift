//! The rust-analyzer probe and the real `[engines.rust]` table, for the
//! end-to-end lane driven through the real `rift` binary.
//!
//! Mirrors `rift-mcp`'s own `rust_engine.rs`; Cargo compiles each crate's
//! integration tests as their own binary, so the module is duplicated
//! rather than shared.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::engine_fixture::EngineFixture;

/// The engine program the live rust fixtures name in `program`.
///
/// The name resolves through `PATH`, where rustup's proxy answers it.
pub(crate) const RUST_ANALYZER_PROGRAM: &str = "rust-analyzer";

/// The variable rustup reads before it resolves a toolchain by directory.
pub(crate) const TOOLCHAIN_VARIABLE: &str = "RUSTUP_TOOLCHAIN";

/// The environment every live rust fixture lays over the inherited one.
///
/// This repository's `rust-toolchain.toml` pins a channel whose installed
/// component set has no `rust-analyzer`, and cargo hands every child it
/// starts - the test binary included - that pinned channel in
/// [`TOOLCHAIN_VARIABLE`]. An engine child inheriting it resolves the
/// pinned channel and fails with `Unknown binary 'rust-analyzer' in
/// official toolchain`. Cleared to the empty value, rustup ignores the
/// variable and resolves by the child's working directory instead; every
/// live fixture is a tempdir outside the repository, so the proxy answers
/// with the default toolchain's component.
pub(crate) fn rust_analyzer_environment() -> BTreeMap<String, String> {
    BTreeMap::from([(TOOLCHAIN_VARIABLE.to_owned(), String::new())])
}

/// Fails the test unless `rust-analyzer` answers `--version` from the
/// fixture tree under the fixture's own environment.
pub(crate) fn require_rust_analyzer(fixture_root: &Path) {
    let probe = std::process::Command::new(RUST_ANALYZER_PROGRAM)
        .arg("--version")
        .envs(rust_analyzer_environment())
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
            "`rust-analyzer --version` failed in {}: install the component on the default \
             toolchain with `rustup component add rust-analyzer` outside this repository. {}",
            fixture_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(error) => panic!(
            "`rust-analyzer` is not on PATH for {}: install the component on the default \
             toolchain with `rustup component add rust-analyzer` outside this repository. \
             {error}",
            fixture_root.display(),
        ),
    }
}

/// The real `[engines.rust]` table, built from [`fixture`].
pub(crate) fn rust_engine_configuration() -> String {
    fixture().configuration_toml()
}

/// The fixture data the shared harness turns into the `[engines.rust]`
/// table: rust-analyzer over the fixture's cargo project, with the
/// toolchain override that lets it start outside this repository's own
/// pinned channel.
pub(crate) fn fixture() -> EngineFixture {
    let mut environment = String::new();
    for (key, value) in rust_analyzer_environment() {
        writeln!(environment, "{key} = \"{value}\"").expect("a string write cannot fail");
    }
    EngineFixture {
        name: "rust",
        program: RUST_ANALYZER_PROGRAM,
        arguments: Vec::new(),
        languages: vec!["rust"],
        extra_toml: format!("\n[engines.rust.environment]\n{environment}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixture_pins_rust_analyzer_1_98() {
        let fixture = super::fixture();
        assert_eq!(fixture.program, "rustup");
        assert_eq!(fixture.arguments, ["run", "1.98", "rust-analyzer"]);
    }
}
