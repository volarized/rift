//! The typescript-language-server probe and the bun project it needs.
//!
//! The engine refuses to initialize without a `typescript` package it can
//! resolve from the workspace root (`-32603 Could not find a valid
//! TypeScript installation`), so the session fixture carries the manifest
//! and the committed lockfile of rift-mcp's fixture project and installs
//! from that lockfile into its tempdir. One project, one lockfile, two
//! suites: the tool-level suite reads the sources beside them, and this
//! one needs only the resolvable package.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use serde_json::json;

use crate::engine_fixture::EngineFixture;

/// The package manager that installs the fixture's pinned `typescript`.
pub(crate) const BUN_PROGRAM: &str = "bun";

/// The runner that resolves and starts the language server package.
pub(crate) const BUNX_PROGRAM: &str = "bunx";

/// The language server, pinned: an unpinned `bunx` argument would float
/// to whatever the registry publishes next.
pub(crate) const LANGUAGE_SERVER_PACKAGE: &str = "typescript-language-server@6.0.0";

/// The manifest and lockfile the install reads, from the one fixture
/// project both live suites share.
pub(crate) fn typescript_package_files() -> [(&'static str, &'static str); 2] {
    [
        (
            "package.json",
            include_str!("../../rift-mcp/tests/fixtures/typescript/package.json"),
        ),
        (
            "bun.lock",
            include_str!("../../rift-mcp/tests/fixtures/typescript/bun.lock"),
        ),
    ]
}

/// Installs the fixture's pinned `typescript` and proves the language
/// server runs, or fails the test with the command's own words.
///
/// Both halves run from the fixture tree, the directory the engine child
/// resolves from. The install reads the committed lockfile, so it resolves
/// nothing and answers from bun's cache when the package is already there.
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
            "`bun install --frozen-lockfile` failed in {}: the fixture's lockfile must resolve \
             the pinned typescript. {}",
            fixture_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(error) => panic!(
            "`{BUN_PROGRAM}` is not on PATH for {}: install bun to run the live typescript \
             suite. {error}",
            fixture_root.display(),
        ),
    }
    let probe = std::process::Command::new(BUNX_PROGRAM)
        .args([LANGUAGE_SERVER_PACKAGE, "--version"])
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
            "`{BUNX_PROGRAM} {LANGUAGE_SERVER_PACKAGE} --version` failed in {}: {}",
            fixture_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(error) => panic!(
            "`{BUNX_PROGRAM}` is not on PATH for {}: install bun to run the live typescript \
             suite. {error}",
            fixture_root.display(),
        ),
    }
}

/// The fixture data the shared harness turns into a launch: the pinned
/// language server started through `bunx`, with one semantic server kept
/// to a single instance so the session contract this suite pins is the
/// one the tool-level suite drives too.
pub(crate) fn fixture() -> EngineFixture {
    EngineFixture {
        program: BUNX_PROGRAM,
        arguments: vec![LANGUAGE_SERVER_PACKAGE.to_owned(), "--stdio".to_owned()],
        environment: BTreeMap::new(),
        initialization_options: Some(json!({
            "tsserver": { "useSyntaxServer": "never" }
        })),
    }
}
