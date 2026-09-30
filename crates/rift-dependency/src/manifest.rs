//! What every lockfile-driven resolver shares about manifests.
//!
//! A resolver receives its manifests as project paths. From each it derives the
//! directory the manifest stands in, the files beside it, and whether a listed
//! manifest in an ancestor directory covers it, and reads each of those files within
//! [`LOCKFILE_BYTES_MAX`]. Each format's model and parse step stay with the resolver
//! that owns the format: each maps its parser's error to
//! [`StaticFileFailure::unparsable`].

use std::fmt;
use std::path::{Component, Path, PathBuf};

use rift_protocol::read::ProjectPath;

use crate::resolver::{FileObservation, LOCKFILE_BYTES_MAX, MANIFESTS_MAX, StaticInputs};

/// The separator between the segments of a project path.
const PATH_SEPARATOR: char = '/';

/// The last segment of a project path: the file name a resolver claims manifests by.
#[must_use]
pub(crate) fn file_name(path: &ProjectPath) -> &str {
    path.0
        .rsplit(PATH_SEPARATOR)
        .next()
        .unwrap_or(path.0.as_str())
}

/// The manifests one resolver reads, and what the manifest bound left out.
pub(crate) struct ClaimedManifests {
    /// The visible paths carrying the resolver's manifest file name, at most
    /// [`MANIFESTS_MAX`] of them in path order.
    pub(crate) manifests: Vec<ProjectPath>,
    /// What the bound dropped, absent when every claimed manifest is read.
    pub(crate) dropped: Option<String>,
}

/// The visible paths carrying `manifest_file_name`, cut to [`MANIFESTS_MAX`].
///
/// Every resolver claims its manifests the same way, so the static context reads one
/// list per resolver and reports one drop.
pub(crate) fn claimed_manifests(
    visible: &[ProjectPath],
    manifest_file_name: &str,
) -> ClaimedManifests {
    let mut manifests: Vec<ProjectPath> = visible
        .iter()
        .filter(|path| file_name(path) == manifest_file_name)
        .cloned()
        .collect();
    let claimed_count = manifests.len();
    manifests.truncate(MANIFESTS_MAX);
    let dropped = (claimed_count > MANIFESTS_MAX).then(|| {
        format!(
            "{} of {claimed_count} {manifest_file_name} manifests were not read: at most \
             {MANIFESTS_MAX} are read per workspace",
            claimed_count - MANIFESTS_MAX
        )
    });
    ClaimedManifests { manifests, dropped }
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

/// The workspace root, lexical and resolved, that a path dependency compares against.
///
/// A path dependency inside the root is project source, which the local index already
/// covers, so no resolver reports it.
pub(crate) struct WorkspacePaths {
    root: PathBuf,
    resolved_root: Option<PathBuf>,
}

impl WorkspacePaths {
    pub(crate) fn new(root: &Path, inputs: &mut dyn StaticInputs) -> Self {
        Self {
            root: lexical(root),
            resolved_root: inputs.canonical_path(root),
        }
    }

    /// Whether `path` lies below the root.
    ///
    /// Where the inputs resolve links, the resolved target decides, since the local
    /// index follows no symlink: a link inside the root pointing outside it is not
    /// project source, and neither is a path where nothing stands. Otherwise the
    /// lexical path decides.
    pub(crate) fn contains(&self, path: &Path, inputs: &mut dyn StaticInputs) -> bool {
        match &self.resolved_root {
            Some(root) => inputs
                .canonical_path(path)
                .is_some_and(|resolved| resolved.starts_with(root)),
            None => lexical(path).starts_with(&self.root),
        }
    }
}

/// `path` with `.` dropped and each `..` taking its parent away, reading no link.
fn lexical(path: &Path) -> PathBuf {
    let mut kept = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                kept.pop();
            }
            other => kept.push(other),
        }
    }
    kept
}

/// Why one static file beside a manifest answered nothing: which file, and what went
/// wrong.
#[derive(Debug)]
pub(crate) struct StaticFileFailure {
    file_name: &'static str,
    cause: StaticFileFailureCause,
}

/// What stopped one static file from answering.
#[derive(Debug)]
enum StaticFileFailureCause {
    /// No such file stands beside the manifest.
    Absent,
    /// The file holds more bytes than `LOCKFILE_BYTES_MAX`.
    OverBound { bytes: u64 },
    /// The file is not the document its tool writes; carries the parser's message.
    Unparsable(String),
}

impl StaticFileFailure {
    /// A file that is not the document its tool writes; carries the parser's message.
    #[must_use]
    pub(crate) const fn unparsable(file_name: &'static str, message: String) -> Self {
        Self {
            file_name,
            cause: StaticFileFailureCause::Unparsable(message),
        }
    }

    /// Whether no such file stood beside the manifest at all.
    #[must_use]
    pub(crate) const fn is_absent(&self) -> bool {
        matches!(self.cause, StaticFileFailureCause::Absent)
    }
}

impl fmt::Display for StaticFileFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file_name = self.file_name;
        match &self.cause {
            StaticFileFailureCause::Absent => write!(formatter, "no {file_name} beside it"),
            StaticFileFailureCause::OverBound { bytes } => write!(
                formatter,
                "{file_name} holds {bytes} bytes, past the {LOCKFILE_BYTES_MAX} byte bound"
            ),
            StaticFileFailureCause::Unparsable(message) => {
                write!(formatter, "{file_name} could not be parsed: {message}")
            }
        }
    }
}

/// Reads the file named `file_name` in `directory`, within `LOCKFILE_BYTES_MAX`.
pub(crate) fn read_static_file(
    directory: &Path,
    file_name: &'static str,
    inputs: &mut dyn StaticInputs,
) -> Result<Vec<u8>, StaticFileFailure> {
    let path = directory.join(file_name);
    let cause = match inputs.read_file(&path, LOCKFILE_BYTES_MAX) {
        FileObservation::Bytes(bytes) => return Ok(bytes),
        FileObservation::Absent => StaticFileFailureCause::Absent,
        FileObservation::OverBound { bytes } => StaticFileFailureCause::OverBound { bytes },
    };
    Err(StaticFileFailure { file_name, cause })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::fixture::RecordedInspector;

    fn project(path: &str) -> ProjectPath {
        ProjectPath(path.to_owned())
    }

    /// `count` claimed manifests, one per directory, beside one file no resolver claims.
    fn probe_manifests(count: usize) -> Vec<ProjectPath> {
        std::iter::once(project("src/lib.rs"))
            .chain((0..count).map(|index| project(&format!("tools/{index:04}/probe.toml"))))
            .collect()
    }

    #[test]
    fn test_claimed_manifests_match_the_file_name_at_any_depth() {
        let visible = [
            project("probe.toml"),
            project("src/lib.rs"),
            project("tools/probe.toml"),
            project("tools/probe.toml.bak"),
        ];
        let claimed = claimed_manifests(&visible, "probe.toml");
        assert_eq!(
            claimed.manifests,
            [project("probe.toml"), project("tools/probe.toml")]
        );
        assert_eq!(claimed.dropped, None);
        assert_eq!(file_name(&project("tools/probe.toml")), "probe.toml");
        assert_eq!(file_name(&project("probe.toml")), "probe.toml");
    }

    #[test]
    fn test_claimed_manifests_read_exactly_manifests_max_without_a_drop() {
        let claimed = claimed_manifests(&probe_manifests(MANIFESTS_MAX), "probe.toml");
        assert_eq!(claimed.manifests.len(), MANIFESTS_MAX);
        assert_eq!(claimed.dropped, None);
    }

    #[test]
    fn test_claimed_manifests_drop_the_manifest_past_the_bound_and_report_it() {
        let claimed = claimed_manifests(&probe_manifests(MANIFESTS_MAX + 1), "probe.toml");
        assert_eq!(claimed.manifests.len(), MANIFESTS_MAX);
        assert_eq!(
            claimed.dropped,
            Some(format!(
                "1 of {} probe.toml manifests were not read: at most {MANIFESTS_MAX} are read \
                 per workspace",
                MANIFESTS_MAX + 1
            ))
        );
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

    /// The lexical form drops a leading `.`, which `Path::components` keeps, and each
    /// `..` takes its parent away.
    #[test]
    fn test_the_lexical_form_drops_current_and_parent_components() {
        assert_eq!(
            lexical(Path::new("./packages/../api/./src")),
            PathBuf::from("api/src")
        );
        assert_eq!(
            lexical(Path::new("/workspace/crates/../../outside")),
            PathBuf::from("/outside")
        );
    }

    #[test]
    fn test_read_static_file_names_the_file_in_every_failure() {
        let oversized = vec![b'#'; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector = RecordedInspector::default()
            .with_directory("/workspace/absent")
            .with_file("/workspace/large/uv.lock", oversized)
            .with_file("/workspace/small/uv.lock", "version = 1\n");

        let absent = read_static_file(Path::new("/workspace/absent"), "uv.lock", &mut inspector)
            .expect_err("no lockfile stands there");
        assert!(absent.is_absent());
        assert_eq!(absent.to_string(), "no uv.lock beside it");

        let large = read_static_file(Path::new("/workspace/large"), "uv.lock", &mut inspector)
            .expect_err("the lockfile is past the bound");
        assert!(!large.is_absent());
        assert_eq!(
            large.to_string(),
            format!(
                "uv.lock holds {} bytes, past the {LOCKFILE_BYTES_MAX} byte bound",
                LOCKFILE_BYTES_MAX + 1
            )
        );

        let bytes = read_static_file(Path::new("/workspace/small"), "uv.lock", &mut inspector)
            .expect("a lockfile within the bound reads whole");
        assert_eq!(bytes, b"version = 1\n");

        let unparsable = StaticFileFailure::unparsable("uv.lock", "expected `=`".to_owned());
        assert!(!unparsable.is_absent());
        assert_eq!(
            unparsable.to_string(),
            "uv.lock could not be parsed: expected `=`"
        );
    }
}
