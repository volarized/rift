//! Records the git commit the `rift` binary is built from, for the product identity.
//!
//! A process names its build by the package version with the commit as semantic-versioning
//! build metadata, so two processes compare builds without reading their executables. The
//! script asks the `git` command line for the commit and for uncommitted changes, and hands
//! both to the compiler as `RIFT_BUILD_COMMIT` and `RIFT_BUILD_DIRTY`; either is empty when it
//! does not apply. `main` passes them to `rift-mcp` as its `BuildCheckout`. A tree with no
//! git, or a crate that does not sit at `crates/rift` in its repository, records neither, and
//! the build carries the package version alone.
//!
//! The script lives in the binary crate because that crate compiles again whenever any crate
//! it links changes; a library crate carrying it would compile again on every edit instead.
//! Cargo re-runs the script when a watched path's modification time moves. The watched paths
//! are every workspace crate's `src` directory, manifest, and build script, the workspace
//! manifest and lockfile, and the git files a commit or checkout rewrites: `HEAD`, the
//! index, the checked-out branch's ref, and `packed-refs`. An edit to any source that reaches
//! the executable therefore re-runs the script and refreshes the dirty mark.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where this crate sits in its repository; any other prefix names a foreign repository.
const CRATE_PREFIX: &str = "crates/rift/";

/// Environment variables that point `git` at another repository or index than the one the
/// crate sits in. A build a git hook runs inherits them.
const REPOSITORY_OVERRIDES: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
];

/// Hex digits a SHA-1 commit id spells; a SHA-256 repository spells 64.
const SHA1_COMMIT_CHARS: usize = 40;
/// Hex digits a SHA-256 commit id spells.
const SHA256_COMMIT_CHARS: usize = 64;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let crate_directory = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let checkout = Checkout::read(&crate_directory);
    let (commit, dirty) = match &checkout {
        Some(checkout) => (checkout.commit.as_str(), checkout.dirty),
        None => ("", false),
    };
    println!("cargo::rustc-env=RIFT_BUILD_COMMIT={commit}");
    println!(
        "cargo::rustc-env=RIFT_BUILD_DIRTY={}",
        if dirty { "true" } else { "" }
    );
    if let Some(checkout) = checkout {
        for watched in checkout.watched_paths() {
            println!("cargo::rerun-if-changed={}", watched.display());
        }
    }
}

/// The checkout this crate is built in, as `git` reports it.
struct Checkout {
    /// The directory `git` runs in.
    crate_directory: PathBuf,
    /// The repository's top-level directory.
    top_level: PathBuf,
    /// The commit `HEAD` names, as lowercase hex.
    commit: String,
    /// Whether the working tree holds changes the commit does not.
    dirty: bool,
}

impl Checkout {
    /// The checkout around `crate_directory`, or `None` when `git` is absent, the directory
    /// is in no repository, the repository has no commit, or the crate sits elsewhere than
    /// `crates/rift` in it.
    fn read(crate_directory: &Path) -> Option<Self> {
        let prefix = git(crate_directory, ["rev-parse", "--show-prefix"])?;
        if prefix != CRATE_PREFIX {
            return None;
        }
        let top_level = PathBuf::from(git(crate_directory, ["rev-parse", "--show-toplevel"])?);
        let commit = git(crate_directory, ["rev-parse", "--verify", "HEAD"])?;
        if !commit_well_formed(&commit) {
            return None;
        }
        // `--no-optional-locks` keeps `status` from refreshing the index, which would move
        // the index's modification time and re-run this script on the next build.
        let changes = git(
            crate_directory,
            ["--no-optional-locks", "status", "--porcelain"],
        )?;
        Some(Self {
            crate_directory: crate_directory.to_owned(),
            top_level,
            commit,
            dirty: !changes.is_empty(),
        })
    }

    /// Every existing path whose change moves the commit, the dirty mark, or the sources the
    /// executable is built from. A path that does not exist is left out: Cargo re-runs a
    /// script on every build while a watched path is missing.
    fn watched_paths(&self) -> Vec<PathBuf> {
        let mut watched = vec![
            self.top_level.join("Cargo.toml"),
            self.top_level.join("Cargo.lock"),
        ];
        watched.extend(crate_sources(&self.top_level.join("crates")));
        watched.extend(self.git_paths());
        watched.retain(|path| path.exists());
        watched
    }

    /// The git files a commit, a checkout, or a staged change rewrites.
    fn git_paths(&self) -> Vec<PathBuf> {
        let mut names = vec![
            "HEAD".to_owned(),
            "index".to_owned(),
            "packed-refs".to_owned(),
        ];
        if let Some(branch) = git(&self.crate_directory, ["symbolic-ref", "-q", "HEAD"]) {
            names.push(branch);
        }
        names
            .iter()
            .filter_map(|name| git(&self.crate_directory, ["rev-parse", "--git-path", name]))
            .map(|path| self.crate_directory.join(path))
            .collect()
    }
}

/// Each workspace crate's `src` directory, manifest, and build script below `crates`.
fn crate_sources(crates: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(crates) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .flat_map(|package| ["src", "Cargo.toml", "build.rs"].map(|member| package.join(member)))
        .collect()
}

/// Whether `commit` spells one SHA-1 or SHA-256 object id as lowercase hex.
fn commit_well_formed(commit: &str) -> bool {
    let length_accepted = [SHA1_COMMIT_CHARS, SHA256_COMMIT_CHARS].contains(&commit.len());
    let charset_accepted = commit
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    length_accepted && charset_accepted
}

/// What `git <arguments>` printed in `directory`, trimmed, or `None` when it could not run
/// or failed.
fn git<Argument: AsRef<OsStr>>(
    directory: &Path,
    arguments: impl IntoIterator<Item = Argument>,
) -> Option<String> {
    let mut command = Command::new("git");
    command.args(arguments).current_dir(directory);
    for variable in REPOSITORY_OVERRIDES {
        command.env_remove(variable);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let printed = String::from_utf8(output.stdout).ok()?;
    Some(printed.trim().to_owned())
}
