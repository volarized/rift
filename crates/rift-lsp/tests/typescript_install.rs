//! The bun project typescript-language-server needs, installed from its lockfile.
//!
//! The engine refuses to initialize without a `typescript` package it can
//! resolve from the workspace root (`-32603 Could not find a valid
//! TypeScript installation`), so a live suite lays out the manifest and the
//! committed lockfile from the fixture project, then installs from that
//! lockfile into its tempdir. The module reads nothing from its crate, so a
//! live suite in another crate reaches it with `#[path]` and installs the
//! same pinned engine.

use std::path::Path;
use std::time::Instant;

/// The package manager that installs the fixture's pinned `typescript`.
pub(crate) const BUN_PROGRAM: &str = "bun";

/// Fixture-local language server executable installed from the lockfile.
pub(crate) const LANGUAGE_SERVER_PROGRAM: &str = "node_modules/.bin/typescript-language-server";

/// The manifest and lockfile the install reads, from the one fixture
/// project the live suites read.
pub(crate) fn typescript_package_files() -> [(&'static str, &'static str); 2] {
    [
        (
            "package.json",
            include_str!("fixtures/typescript/package.json"),
        ),
        ("bun.lock", include_str!("fixtures/typescript/bun.lock")),
    ]
}

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
