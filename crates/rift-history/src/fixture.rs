//! Deterministic git fixtures for tests across Rift crates.
//!
//! Every command runs with a fixed identity and clock and with signing off,
//! so fixture repositories hash identically across machines and never touch
//! the developer's gpg configuration.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// One git invocation in `root`, with the fixture identity, clock, and
/// signing-off configuration every fixture command shares.
fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
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
        ]);
    command
}

/// Runs one git command in `root`, panicking on failure.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero — a fixture that cannot be
/// built fails the test that needs it.
pub fn git(root: &Path, arguments: &[&str]) {
    let status = command(root)
        .args(arguments)
        .status()
        .expect("git must run");
    assert!(status.success(), "git {arguments:?} must succeed");
}

/// Runs one git plumbing command in `root` with `stdin` bytes, returning its
/// trimmed stdout.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
fn plumb(root: &Path, arguments: &[&str], stdin: &[u8]) -> String {
    let mut child = command(root)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git must spawn");
    child
        .stdin
        .take()
        .expect("stdin must be piped")
        .write_all(stdin)
        .expect("stdin must accept the fixture bytes");
    let output = child.wait_with_output().expect("git must run");
    assert!(output.status.success(), "git {arguments:?} must succeed");
    String::from_utf8(output.stdout)
        .expect("plumbing output must be UTF-8")
        .trim()
        .to_owned()
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

/// Commits one blob at a raw byte path, reachable as the ref `branch`.
///
/// The tree is built through git plumbing, so the path never touches the
/// host filesystem — the way a spelling the platform forbids, or bytes that
/// are not UTF-8, still become a committed tree entry.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn commit_raw_path(root: &Path, raw_path: &[u8], branch: &str) {
    let blob = plumb(root, &["hash-object", "-w", "--stdin"], b"pub fn raw() {}\n");
    let mut entry = format!("100644 blob {blob}\t").into_bytes();
    entry.extend_from_slice(raw_path);
    entry.push(b'\n');
    let tree = plumb(root, &["mktree"], &entry);
    let commit = plumb(root, &["commit-tree", &tree, "-m", "raw path"], b"");
    git(root, &["update-ref", branch, &commit]);
}
