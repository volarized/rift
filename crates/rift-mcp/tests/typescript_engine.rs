//! Typescript-language-server probe and real TypeScript language LSP configuration.
//!
//! The engine refuses to initialize without a `typescript` package it can
//! resolve from the workspace root (`-32603 Could not find a valid
//! TypeScript installation`), so the fixture carries a `package.json` and
//! a committed `bun.lock`, and the suite installs from that lockfile into
//! its tempdir copy. The install never touches the repository checkout,
//! and a warm bun cache serves it without the registry.

use std::path::Path;
use std::time::Instant;

use crate::engine_fixture::EngineFixture;

/// The package manager that installs the fixture's pinned `typescript`.
pub(crate) const BUN_PROGRAM: &str = "bun";

/// Fixture-local language server executable installed from the lockfile.
pub(crate) const LANGUAGE_SERVER_PROGRAM: &str = "node_modules/.bin/typescript-language-server";

/// Installs pinned fixture packages and checks the local language server.
///
/// Commands run from isolated fixture tree. Frozen install accepts only
/// committed lockfile. Version check invokes local executable directly,
/// so tests do not depend on a package runner cache or PATH lookup.
pub(crate) fn install_typescript_engine(fixture_root: &Path) {
    let started = Instant::now();
    let install = std::process::Command::new(BUN_PROGRAM)
        .args(["install", "--frozen-lockfile"])
        .current_dir(fixture_root)
        .output();
    match install {
        Ok(output) if output.status.success() => {
            eprintln!("bun install: {:?}", started.elapsed());
        }
        Ok(output) => panic!(
            "`bun install --frozen-lockfile` failed in {}: fixture lockfile must install pinned packages. {}",
            fixture_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(error) => panic!(
            "`{BUN_PROGRAM}` is not on PATH for {}: install bun to run live typescript tests. {error}",
            fixture_root.display(),
        ),
    }
    let probe = std::process::Command::new(LANGUAGE_SERVER_PROGRAM)
        .arg("--version")
        .current_dir(fixture_root)
        .output();
    match probe {
        Ok(output) if output.status.success() => {
            eprintln!(
                "typescript-language-server: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        Ok(output) => panic!(
            "`{LANGUAGE_SERVER_PROGRAM} --version` failed in {}: {}",
            fixture_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(error) => panic!(
            "`{LANGUAGE_SERVER_PROGRAM}` is not installed in {}: run frozen fixture install first. {error}",
            fixture_root.display(),
        ),
    }
}

/// Real TypeScript language LSP configuration, built from [`fixture`], beside
/// the `[source]` policy the fixture always needs.
pub(crate) fn typescript_engine_configuration() -> String {
    format!(
        "{SOURCE_EXCLUDES_NODE_MODULES}\n{}",
        fixture().configuration_toml()
    )
}

/// The fixture data used for the local TypeScript engine.
pub(crate) fn fixture() -> EngineFixture {
    EngineFixture {
        name: "typescript",
        program: LANGUAGE_SERVER_PROGRAM,
        arguments: vec!["--stdio"],
        languages: vec!["typescript", "typescript:tsx"],
        extra_toml: "\n[lsp.typescript.initialization_options.tsserver]\n\
                     useSyntaxServer = \"never\"\n"
            .to_owned(),
    }
}

/// The `[source]` policy this engine's fixture always needs beside its
/// TypeScript LSP configuration: without it the walk reaches
/// `typescript`'s own 23mb of installed sources and the first change
/// refuses with `violation file_too_large`.
pub(crate) const SOURCE_EXCLUDES_NODE_MODULES: &str = "[source]\nexclude = [\"node_modules/**\"]\n";
