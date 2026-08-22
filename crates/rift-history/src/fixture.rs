//! Deterministic git fixtures for tests across Rift crates.
//!
//! Every command runs with a fixed identity and clock and with signing off,
//! so fixture repositories hash identically across machines and never touch
//! the developer's gpg configuration.

use std::path::Path;
use std::process::Command;

/// Runs one git command in `root`, panicking on failure.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero — a fixture that cannot be
/// built fails the test that needs it.
pub fn git(root: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Rift Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@rift.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00 +0000")
        .env("GIT_COMMITTER_NAME", "Rift Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@rift.invalid")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00 +0000")
        .args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ])
        .args(arguments)
        .status()
        .expect("git must run");
    assert!(status.success(), "git {arguments:?} must succeed");
}

/// Initializes a repository in `root` on branch `main`.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn init(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
}

/// Stages everything in `root` and commits it with `message`.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn commit_all(root: &Path, message: &str) {
    git(root, &["add", "--all"]);
    git(root, &["commit", "-q", "-m", message]);
}
