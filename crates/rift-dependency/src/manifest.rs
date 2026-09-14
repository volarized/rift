//! What every lockfile-driven resolver shares about manifests and answers.
//!
//! A resolver receives its manifests as project paths. From each it derives the
//! directory the manifest stands in, the lockfile beside it, and whether a listed
//! manifest in an ancestor directory covers it; reads that lockfile within
//! [`LOCKFILE_BYTES_MAX`]; and assembles its answer through a [`ResolutionBuilder`]
//! that stops at [`PACKAGES_MAX`]. The lockfile's model and parse step stay with the
//! resolver that owns the format: each maps its parser's error to
//! [`LockfileFailure::unparsable`].

use std::fmt;
use std::path::{Path, PathBuf};

use rift_protocol::read::{PackageIdentity, ProjectPath};

use crate::catalog::{CatalogEntry, Resolution};
use crate::resolver::{FileObservation, Inspector, LOCKFILE_BYTES_MAX, PACKAGES_MAX};

/// The separator between the segments of a project path.
const PATH_SEPARATOR: char = '/';

/// One resolution being assembled, refusing entries past `PACKAGES_MAX` and counting them.
#[derive(Default)]
pub(crate) struct ResolutionBuilder {
    resolution: Resolution,
    dropped_count: usize,
}

impl ResolutionBuilder {
    /// Records one visible workspace path the answer depends on.
    pub(crate) fn input(&mut self, path: ProjectPath) {
        self.resolution.inputs.push(path);
    }

    /// Records one thing the resolver could not do.
    pub(crate) fn degradation(&mut self, reason: String) {
        self.resolution.degradations.push(reason);
    }

    /// Takes one entry, or counts it dropped once `PACKAGES_MAX` entries stand.
    pub(crate) fn entry(&mut self, entry: CatalogEntry) {
        if self.resolution.entries.len() < PACKAGES_MAX {
            self.resolution.entries.push(entry);
        } else {
            self.dropped_count += 1;
        }
    }

    /// Takes every entry in order, under the same bound as `entry`.
    pub(crate) fn entries(&mut self, entries: impl IntoIterator<Item = CatalogEntry>) {
        for entry in entries {
            self.entry(entry);
        }
    }

    /// The finished resolution: entries in identity order, the drop reported last.
    #[must_use]
    pub(crate) fn build(mut self) -> Resolution {
        if self.dropped_count > 0 {
            let total = self.resolution.entries.len() + self.dropped_count;
            self.degradation(format!(
                "{} of {total} packages were not cataloged: at most {PACKAGES_MAX} are \
                 cataloged per workspace",
                self.dropped_count
            ));
        }
        self.resolution.entries.sort_by(|left, right| {
            identity_order(left.identity()).cmp(&identity_order(right.identity()))
        });
        self.resolution
    }
}

/// The order entries sort in: manager, then name, then version.
fn identity_order(identity: &PackageIdentity) -> (&str, &str, &str) {
    (&identity.manager, &identity.name, &identity.version)
}

/// The manifests with no other listed manifest in an ancestor directory, in path order.
///
/// Every manifest pair is compared, so the work is quadratic in the manifest count,
/// which `MANIFESTS_MAX` bounds.
#[must_use]
pub(crate) fn top_level_manifests(manifests: &[ProjectPath]) -> Vec<&ProjectPath> {
    manifests
        .iter()
        .filter(|manifest| {
            let directory = manifest_directory(manifest);
            !manifests
                .iter()
                .any(|other| is_ancestor_directory(manifest_directory(other), directory))
        })
        .collect()
}

/// The directory holding a manifest, project-relative; empty for the workspace root.
#[must_use]
pub(crate) fn manifest_directory(manifest: &ProjectPath) -> &str {
    manifest
        .0
        .rsplit_once(PATH_SEPARATOR)
        .map_or("", |(directory, _)| directory)
}

/// Whether `ancestor` is a proper ancestor directory of `directory`.
#[must_use]
pub(crate) fn is_ancestor_directory(ancestor: &str, directory: &str) -> bool {
    match ancestor {
        "" => !directory.is_empty(),
        ancestor => directory
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with(PATH_SEPARATOR)),
    }
}

/// The absolute directory holding a manifest.
#[must_use]
pub(crate) fn manifest_directory_path(root: &Path, manifest: &ProjectPath) -> PathBuf {
    match manifest_directory(manifest) {
        "" => root.to_path_buf(),
        directory => root.join(directory),
    }
}

/// The project path of `file_name` beside a manifest.
#[must_use]
pub(crate) fn file_beside(manifest: &ProjectPath, file_name: &str) -> ProjectPath {
    match manifest_directory(manifest) {
        "" => ProjectPath(file_name.to_owned()),
        directory => ProjectPath(format!("{directory}{PATH_SEPARATOR}{file_name}")),
    }
}

/// Why the lockfile beside a manifest answered nothing: which file, and what went wrong.
#[derive(Debug)]
pub(crate) struct LockfileFailure {
    file_name: &'static str,
    cause: LockfileFailureCause,
}

/// What stopped one lockfile from answering.
#[derive(Debug)]
enum LockfileFailureCause {
    /// No lockfile stands beside the manifest.
    Absent,
    /// The lockfile holds more bytes than `LOCKFILE_BYTES_MAX`.
    OverBound { bytes: u64 },
    /// The lockfile is not the document its tool writes; carries the parser's message.
    Unparsable(String),
}

impl LockfileFailure {
    /// A lockfile that is not the document its tool writes; carries the parser's message.
    #[must_use]
    pub(crate) const fn unparsable(file_name: &'static str, message: String) -> Self {
        Self {
            file_name,
            cause: LockfileFailureCause::Unparsable(message),
        }
    }

    /// Whether no lockfile stood beside the manifest at all.
    #[must_use]
    pub(crate) const fn is_absent(&self) -> bool {
        matches!(self.cause, LockfileFailureCause::Absent)
    }
}

impl fmt::Display for LockfileFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file_name = self.file_name;
        match &self.cause {
            LockfileFailureCause::Absent => write!(formatter, "no {file_name} beside it"),
            LockfileFailureCause::OverBound { bytes } => write!(
                formatter,
                "{file_name} holds {bytes} bytes, past the {LOCKFILE_BYTES_MAX} byte bound"
            ),
            LockfileFailureCause::Unparsable(message) => {
                write!(formatter, "{file_name} could not be parsed: {message}")
            }
        }
    }
}

/// Reads the lockfile named `file_name` in `directory`, within `LOCKFILE_BYTES_MAX`.
pub(crate) fn read_lockfile(
    directory: &Path,
    file_name: &'static str,
    inspector: &mut dyn Inspector,
) -> Result<Vec<u8>, LockfileFailure> {
    let path = directory.join(file_name);
    let cause = match inspector.read_file(&path, LOCKFILE_BYTES_MAX) {
        FileObservation::Bytes(bytes) => return Ok(bytes),
        FileObservation::Absent => LockfileFailureCause::Absent,
        FileObservation::OverBound { bytes } => LockfileFailureCause::OverBound { bytes },
    };
    Err(LockfileFailure { file_name, cause })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rift_protocol::read::Language;

    use super::*;
    use crate::catalog::package_identity;
    use crate::fixture::RecordedInspector;

    fn project(path: &str) -> ProjectPath {
        ProjectPath(path.to_owned())
    }

    fn entry(manager: &str, name: &str, version: &str) -> CatalogEntry {
        let language = Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        CatalogEntry::dependency(
            package_identity(manager, name, version),
            language,
            None,
            false,
        )
    }

    #[test]
    fn test_top_level_manifests_keeps_only_uncovered_directories() {
        let manifests = [
            project("Cargo.toml"),
            project("crates/a/Cargo.toml"),
            project("tools/x/Cargo.toml"),
        ];
        let top_level: Vec<&str> = top_level_manifests(&manifests)
            .into_iter()
            .map(|manifest| manifest.0.as_str())
            .collect();
        assert_eq!(top_level, ["Cargo.toml"]);

        let siblings = [
            project("crates/a/Cargo.toml"),
            project("crates/ab/Cargo.toml"),
            project("crates/a/nested/Cargo.toml"),
        ];
        let top_level: Vec<&str> = top_level_manifests(&siblings)
            .into_iter()
            .map(|manifest| manifest.0.as_str())
            .collect();
        assert_eq!(top_level, ["crates/a/Cargo.toml", "crates/ab/Cargo.toml"]);
    }

    #[test]
    fn test_is_ancestor_directory_requires_a_proper_prefix_segment() {
        assert!(is_ancestor_directory("", "tools"));
        assert!(!is_ancestor_directory("", ""));
        assert!(is_ancestor_directory("apps", "apps/api"));
        assert!(!is_ancestor_directory("apps", "apps"));
        assert!(!is_ancestor_directory("apps", "apps-legacy/api"));
        assert!(!is_ancestor_directory("apps/api", "apps"));
    }

    #[test]
    fn test_file_beside_and_manifest_directory_path_follow_the_manifest() {
        assert_eq!(
            file_beside(&project("Cargo.toml"), "Cargo.lock"),
            project("Cargo.lock")
        );
        assert_eq!(
            file_beside(&project("crates/a/Cargo.toml"), "Cargo.lock"),
            project("crates/a/Cargo.lock")
        );
        let root = Path::new("/workspace");
        assert_eq!(
            manifest_directory_path(root, &project("Cargo.toml")),
            PathBuf::from("/workspace")
        );
        assert_eq!(
            manifest_directory_path(root, &project("crates/a/Cargo.toml")),
            PathBuf::from("/workspace/crates/a")
        );
    }

    #[test]
    fn test_build_sorts_by_identity_and_reports_entries_dropped_past_packages_max() {
        let mut answer = ResolutionBuilder::default();
        answer.entry(entry("pypi", "typer", "0.27.1"));
        answer.entry(entry("cargo", "serde", "1.0.228"));
        answer.entries(
            (0..PACKAGES_MAX - 2).map(|index| entry("npm", &format!("pkg-{index:05}"), "1.0.0")),
        );
        answer.entry(entry("cargo", "dropped", "0.0.0"));

        let resolution = answer.build();

        assert_eq!(resolution.entries.len(), PACKAGES_MAX);
        assert_eq!(
            resolution.entries[0].identity().name,
            "serde",
            "cargo sorts first, and the dropped cargo entry never joined"
        );
        assert_eq!(
            resolution.entries[PACKAGES_MAX - 1].identity().name,
            "typer"
        );
        assert_eq!(
            resolution.degradations,
            [format!(
                "1 of {} packages were not cataloged: at most {PACKAGES_MAX} are cataloged per \
                 workspace",
                PACKAGES_MAX + 1
            )]
        );
    }

    #[test]
    fn test_read_lockfile_names_the_file_in_every_failure() {
        let oversized = vec![b'#'; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector = RecordedInspector::default()
            .with_directory("/workspace/absent")
            .with_file("/workspace/large/uv.lock", oversized)
            .with_file("/workspace/small/uv.lock", "version = 1\n");

        let absent = read_lockfile(Path::new("/workspace/absent"), "uv.lock", &mut inspector)
            .expect_err("no lockfile stands there");
        assert!(absent.is_absent());
        assert_eq!(absent.to_string(), "no uv.lock beside it");

        let large = read_lockfile(Path::new("/workspace/large"), "uv.lock", &mut inspector)
            .expect_err("the lockfile is past the bound");
        assert!(!large.is_absent());
        assert_eq!(
            large.to_string(),
            format!(
                "uv.lock holds {} bytes, past the {LOCKFILE_BYTES_MAX} byte bound",
                LOCKFILE_BYTES_MAX + 1
            )
        );

        let bytes = read_lockfile(Path::new("/workspace/small"), "uv.lock", &mut inspector)
            .expect("a lockfile within the bound reads whole");
        assert_eq!(bytes, b"version = 1\n");

        let unparsable = LockfileFailure::unparsable("uv.lock", "expected `=`".to_owned());
        assert!(!unparsable.is_absent());
        assert_eq!(
            unparsable.to_string(),
            "uv.lock could not be parsed: expected `=`"
        );
    }
}
