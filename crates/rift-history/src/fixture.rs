//! Deterministic git fixtures for tests across Rift crates.
//!
//! Every command runs with a fixed identity and clock and with signing off,
//! so fixture repositories hash identically across machines and never touch
//! the developer's gpg configuration.

use std::fmt::Write as _;
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
/// Panics when git cannot run or exits nonzero - a fixture that cannot be
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

/// Stages everything in `root` and commits it with `message`, authored and committed at
/// `date`, a date git reads such as `2026-01-01T00:01:00 +0000`, so a fixture orders its
/// commits by time.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn commit_all_at(root: &Path, message: &str, date: &str) {
    git(root, &["add", "--all"]);
    let status = command(root)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .args(["commit", "-q", "-m", message])
        .status()
        .expect("git must run");
    assert!(status.success(), "git commit at {date} must succeed");
}

/// Commits a tree naming a subtree the object store does not hold, reachable
/// as the ref `branch`.
///
/// Plumbing writes the tree entry without resolving it, so the commit is the
/// shape a truncated or corrupted object store leaves behind: every read that
/// descends into that subtree fails.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn commit_missing_subtree(root: &Path, branch: &str) {
    commit_missing_object(root, "040000 tree", "absent", branch);
}

/// Commits a tree naming, at `path`, a blob the object store does not hold,
/// reachable as the ref `branch`: every read of that file's bytes fails.
/// `path` is one entry of the root tree, so it holds no `/`.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn commit_missing_blob(root: &Path, path: &str, branch: &str) {
    commit_missing_object(root, "100644 blob", path, branch);
}

/// Commits a tree whose one entry, `mode_and_kind` at `path`, names an object
/// the object store does not hold, reachable as the ref `branch`.
fn commit_missing_object(root: &Path, mode_and_kind: &str, path: &str, branch: &str) {
    let absent = "0123456789abcdef0123456789abcdef01234567";
    let entry = format!("{mode_and_kind} {absent}\t{path}\n");
    let tree = plumb(root, &["mktree", "--missing"], entry.as_bytes());
    let commit = plumb(root, &["commit-tree", &tree, "-m", "missing object"], b"");
    git(root, &["update-ref", branch, &commit]);
}

/// Commits a tree that names `path` twice, each entry with a blob of its own,
/// reachable as the ref `branch`.
///
/// `git mktree` writes the entries without checking for a repeated name, so
/// the commit is the shape `git fsck` reports as `duplicateEntries`.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn commit_duplicate_path(root: &Path, path: &str, branch: &str) {
    let first = plumb(
        root,
        &["hash-object", "-w", "--stdin"],
        b"pub fn first() {}\n",
    );
    let second = plumb(
        root,
        &["hash-object", "-w", "--stdin"],
        b"pub fn second() {}\n",
    );
    let entries = format!("100644 blob {first}\t{path}\n100644 blob {second}\t{path}\n");
    let tree = plumb(root, &["mktree"], entries.as_bytes());
    let commit = plumb(root, &["commit-tree", &tree, "-m", "duplicate path"], b"");
    git(root, &["update-ref", branch, &commit]);
}

/// Commits `count` files `wide/<index>.txt` naming one blob, on top of the
/// commit `branch` names, and moves `branch` to it.
///
/// The tree is written through git plumbing, so a commit changing more paths
/// than a comparison's bound costs one blob and one tree, not one file on
/// disk per path.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn commit_wide_folder(root: &Path, count: usize, branch: &str) {
    let blob = plumb(root, &["hash-object", "-w", "--stdin"], b"wide\n");
    let mut entries = String::new();
    for index in 0..count {
        writeln!(entries, "100644 blob {blob}\t{index:06}.txt")
            .expect("a String takes every write");
    }
    let wide = plumb(root, &["mktree"], entries.as_bytes());
    let parent = plumb(root, &["rev-parse", branch], b"");
    let parent_tree = plumb(root, &["rev-parse", &format!("{parent}^{{tree}}")], b"");
    let mut listing = plumb(root, &["ls-tree", &parent_tree], b"");
    writeln!(listing, "\n040000 tree {wide}\twide").expect("a String takes every write");
    let tree = plumb(root, &["mktree"], listing.trim_start().as_bytes());
    let commit = plumb(
        root,
        &["commit-tree", &tree, "-p", &parent, "-m", "wide folder"],
        b"",
    );
    git(root, &["update-ref", branch, &commit]);
}

/// Commits a symbolic link at `path` naming `target`, in place of the entry
/// the commit `branch` names holds there, on top of that commit, and moves
/// `branch` to it. `path` is one entry of the root tree, so it holds no `/`.
///
/// The tree is written through git plumbing, so the link never touches the
/// host filesystem and the fixture builds alike on a platform where creating
/// a symbolic link needs a privilege.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn commit_symlink_in_place(root: &Path, path: &str, target: &str, branch: &str) {
    let link = plumb(root, &["hash-object", "-w", "--stdin"], target.as_bytes());
    let parent = plumb(root, &["rev-parse", branch], b"");
    let parent_tree = plumb(root, &["rev-parse", &format!("{parent}^{{tree}}")], b"");
    let listing = plumb(root, &["ls-tree", &parent_tree], b"");
    let replaced = format!("\t{path}");
    let mut entries = String::new();
    for entry in listing.lines().filter(|entry| !entry.ends_with(&replaced)) {
        writeln!(entries, "{entry}").expect("a String takes every write");
    }
    writeln!(entries, "120000 blob {link}\t{path}").expect("a String takes every write");
    let tree = plumb(root, &["mktree"], entries.as_bytes());
    let commit = plumb(
        root,
        &["commit-tree", &tree, "-p", &parent, "-m", "link in place"],
        b"",
    );
    git(root, &["update-ref", branch, &commit]);
}

/// Commits one blob at a raw byte path, reachable as the ref `branch`.
///
/// The tree is built through git plumbing, so the path never touches the
/// host filesystem - the way a spelling the platform forbids, or bytes that
/// are not UTF-8, still become a committed tree entry.
///
/// # Panics
///
/// Panics when git cannot run or exits nonzero.
pub fn commit_raw_path(root: &Path, raw_path: &[u8], branch: &str) {
    let blob = plumb(
        root,
        &["hash-object", "-w", "--stdin"],
        b"pub fn raw() {}\n",
    );
    let mut entry = format!("100644 blob {blob}\t").into_bytes();
    entry.extend_from_slice(raw_path);
    entry.push(b'\n');
    let tree = plumb(root, &["mktree"], &entry);
    let commit = plumb(root, &["commit-tree", &tree, "-m", "raw path"], b"");
    git(root, &["update-ref", branch, &commit]);
}
