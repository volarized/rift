//! The tombi probe and the real `[engines.toml]` table.

use std::path::Path;

use crate::engine_fixture::EngineFixture;

/// The engine program the live TOML fixture names in `program`.
///
/// The name resolves through `PATH`; the CI job installs it with
/// `uv tool install tombi`, and a local run expects the same.
pub(crate) const TOMBI_PROGRAM: &str = "tombi";

/// The subcommand that starts tombi's language server over stdio.
pub(crate) const TOMBI_LSP_ARGUMENT: &str = "lsp";

/// Fails the test unless `tombi` answers `--version` from the fixture tree.
pub(crate) fn require_tombi(fixture_root: &Path) {
    let probe = std::process::Command::new(TOMBI_PROGRAM)
        .arg("--version")
        .current_dir(fixture_root)
        .output();
    match probe {
        Ok(output) if output.status.success() => {
            eprintln!("tombi: {}", String::from_utf8_lossy(&output.stdout).trim());
        }
        Ok(output) => panic!(
            "`tombi --version` failed in {}: install it with `uv tool install tombi`. {}",
            fixture_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(error) => panic!(
            "`{TOMBI_PROGRAM}` is not on PATH for {}: install it with `uv tool install tombi`. \
             {error}",
            fixture_root.display(),
        ),
    }
}

/// The real `[engines.toml]` table, built from [`fixture`].
pub(crate) fn toml_engine_configuration() -> String {
    fixture().configuration_toml()
}

/// The fixture data the shared harness turns into the `[engines.toml]`
/// table: tombi's language server over the fixture tree.
///
/// tombi answers `initialize` and a first `textDocument/diagnostic` pull
/// immediately - it has no background project load to announce over
/// `$/progress` the way rust-analyzer does - so the suite makes one call
/// with no warm-up loop.
pub(crate) fn fixture() -> EngineFixture {
    EngineFixture {
        name: "toml",
        program: TOMBI_PROGRAM,
        arguments: vec![TOMBI_LSP_ARGUMENT],
        languages: vec!["toml"],
        extra_toml: String::new(),
    }
}
