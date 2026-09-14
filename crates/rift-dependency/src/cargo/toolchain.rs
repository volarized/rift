//! The standard library: one entry per resolution from the toolchain `rustc` reports.

use std::path::{Path, PathBuf};

use super::{
    RunFailure, RunFailureCause, first_line, package_identity, run_failure, run_to_success,
    rust_language, toolchain_command,
};
use crate::catalog::{CatalogEntry, PackageLocation, STDLIB_MANAGER};
use crate::manifest::ResolutionBuilder;
use crate::resolver::Inspector;

/// The Rust compiler executable, resolved on the inspector's `PATH`.
const RUSTC_PROGRAM: &str = "rustc";
/// The `rustc` arguments printing the toolchain's sysroot.
const SYSROOT_ARGUMENTS: [&str; 2] = ["--print", "sysroot"];
/// The `rustc` arguments printing the toolchain's version line.
const VERSION_ARGUMENTS: [&str; 1] = ["--version"];
/// The standard library's package name under `STDLIB_MANAGER`.
const STDLIB_PACKAGE_NAME: &str = "rust";
/// The standard library source below a sysroot, present with the `rust-src` component.
pub(super) const SYSROOT_LIBRARY_PATH: &str = "lib/rustlib/src/rust/library";

/// The toolchain `rustc` reports: its sysroot and version.
#[derive(Debug)]
struct Toolchain {
    sysroot: PathBuf,
    version: String,
}

/// Catalogs the toolchain's standard library once, from `rustc`'s sysroot and version.
///
/// The entry carries the library source root only when the sysroot holds it; a
/// toolchain without the `rust-src` component still names its version.
pub(super) fn resolve_stdlib(
    root: &Path,
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    match observe_toolchain(root, inspector) {
        Ok(toolchain) => {
            let library = toolchain.sysroot.join(SYSROOT_LIBRARY_PATH);
            let identity =
                package_identity(STDLIB_MANAGER, STDLIB_PACKAGE_NAME, &toolchain.version);
            let mut entry = CatalogEntry::new(identity, PackageLocation::Stdlib, rust_language());
            if inspector.directory_exists(&library) {
                entry = entry.with_source_root(library);
            }
            answer.entry(entry);
        }
        Err(failure) => answer.degradation(format!(
            "{RUSTC_PROGRAM} unavailable ({failure}); no standard library entry"
        )),
    }
}

/// Runs `rustc` for its sysroot, then its version, from the workspace root.
fn observe_toolchain(root: &Path, inspector: &mut dyn Inspector) -> Result<Toolchain, RunFailure> {
    let sysroot = rustc_sysroot(root, inspector)?;
    let version = rustc_version(root, inspector)?;
    Ok(Toolchain { sysroot, version })
}

/// The sysroot `rustc --print sysroot` names, trimmed of its line ending.
fn rustc_sysroot(root: &Path, inspector: &mut dyn Inspector) -> Result<PathBuf, RunFailure> {
    let command = toolchain_command(RUSTC_PROGRAM, &SYSROOT_ARGUMENTS, root.to_path_buf());
    let output = run_to_success(&command, inspector)?;
    match output.stdout.trim() {
        "" => Err(run_failure(&command, RunFailureCause::EmptySysroot)),
        sysroot => Ok(PathBuf::from(sysroot)),
    }
}

/// The version token of `rustc --version`: the second word of its first line.
fn rustc_version(root: &Path, inspector: &mut dyn Inspector) -> Result<String, RunFailure> {
    let command = toolchain_command(RUSTC_PROGRAM, &VERSION_ARGUMENTS, root.to_path_buf());
    let output = run_to_success(&command, inspector)?;
    let line = first_line(&output.stdout);
    line.split_whitespace()
        .nth(1)
        .map(str::to_owned)
        .ok_or_else(|| run_failure(&command, RunFailureCause::MissingVersion(line.to_owned())))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::fixture::{ROOT, SYSROOT, entry, resolve, with_rustc};
    use crate::catalog::PackageLocation;
    use crate::fixture::RecordedInspector;

    #[test]
    fn test_resolve_stdlib_present_catalogs_library_root() {
        let mut inspector = with_rustc(RecordedInspector::default().with_directory(ROOT));

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        let stdlib = entry(&resolution, "rust");
        assert_eq!(stdlib.identity().manager, "stdlib");
        assert_eq!(stdlib.identity().version, "1.98.0");
        assert_eq!(stdlib.location(), PackageLocation::Stdlib);
        assert_eq!(
            stdlib.source_root(),
            Some(Path::new("/toolchain/lib/rustlib/src/rust/library"))
        );
        assert!(!stdlib.is_direct());
        assert_eq!(stdlib.language().identity_segment(), "rust");
        assert!(
            inspector
                .asked
                .contains(&format!("run rustc --print sysroot in {ROOT}"))
        );
        assert!(
            inspector
                .asked
                .contains(&format!("run rustc --version in {ROOT}"))
        );
    }

    #[test]
    fn test_resolve_stdlib_library_absent_catalogs_without_root() {
        let mut inspector = RecordedInspector::default()
            .with_directory(ROOT)
            .with_command(
                "rustc --print sysroot",
                RecordedInspector::succeeded(format!("{SYSROOT}\n")),
            )
            .with_command(
                "rustc --version",
                RecordedInspector::succeeded("rustc 1.98.0 (88d9e12ae 2026-08-18)\n"),
            );

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        let stdlib = entry(&resolution, "rust");
        assert_eq!(stdlib.identity().version, "1.98.0");
        assert_eq!(stdlib.source_root(), None);
    }

    #[test]
    fn test_resolve_rustc_unavailable_degrades_without_stdlib_entry() {
        let mut inspector = RecordedInspector::default().with_directory(ROOT);

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert!(
            resolution
                .entries
                .iter()
                .all(|entry| entry.identity().manager != "stdlib")
        );
        assert_eq!(
            resolution.degradations.last().map(String::as_str),
            Some(
                "rustc unavailable (`rustc --print sysroot` could not run: failed to launch: \
                 No such file or directory (os error 2)); no standard library entry"
            ),
            "the standard library is resolved after every manifest"
        );
    }

    #[test]
    fn test_resolve_rustc_version_without_token_degrades_without_stdlib_entry() {
        let mut inspector = RecordedInspector::default()
            .with_directory(ROOT)
            .with_command(
                "rustc --print sysroot",
                RecordedInspector::succeeded(format!("{SYSROOT}\n")),
            )
            .with_command("rustc --version", RecordedInspector::succeeded("rustc\n"));

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert!(resolution.entries.is_empty());
        assert_eq!(
            resolution.degradations.last().map(String::as_str),
            Some(
                "rustc unavailable (`rustc --version` answered `rustc` without a version \
                 token); no standard library entry"
            )
        );
    }

    #[test]
    fn test_resolve_rustc_empty_sysroot_degrades_without_stdlib_entry() {
        let mut inspector = RecordedInspector::default()
            .with_directory(ROOT)
            .with_command("rustc --print sysroot", RecordedInspector::succeeded("\n"));

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert!(resolution.entries.is_empty());
        assert_eq!(
            resolution.degradations.last().map(String::as_str),
            Some(
                "rustc unavailable (`rustc --print sysroot` answered an empty sysroot); no \
                 standard library entry"
            )
        );
    }
}
