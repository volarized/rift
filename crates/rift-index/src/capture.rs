//! Stat-keyed reuse of request-time capture digests.
//!
//! Index validation captures the workspace on every request, and the capture is the
//! content comparison: a read accepts a publication only when the captured digests fold
//! to its fingerprint. Reading and hashing every file on every request costs the whole
//! tree's bytes, so each capture records every path's stat beside the digests it
//! captured, and the next capture reads and hashes only the paths whose stat moved.

mod selection;
pub(crate) use selection::{DiscoveryInputs, SelectionInputs};

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use rift_core::ProjectPath;
use rift_core::constants::RIFT_STATE_DIRECTORY;
use tempfile::NamedTempFile;

use crate::change_set::FileDigest;
use crate::workspace::{WorkspaceIndexLimits, metadata_is_executable, read_file_bytes};
use rift_error::{RiftError, errors};
use same_file::Handle;

/// Root identity retained while metadata observations are reused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RootIdentity(Arc<Handle>);

#[cfg(test)]
pub(crate) fn supports_stat_reuse(root: &Path) -> bool {
    CaptureBoundary::create(root).is_some()
}

/// A timestamped file on the workspace filesystem, created before capture reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureBoundary {
    modified: SystemTime,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(unix)]
    device: u64,
}

impl CaptureBoundary {
    /// Creates a boundary under `.rift`; failure disables reuse for this capture.
    pub(crate) fn create(root: &Path) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;

        let root_metadata = fs::metadata(root).ok()?;
        if !root_metadata.is_dir() {
            return None;
        }
        let directory = root.join(RIFT_STATE_DIRECTORY);
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
        let directory_metadata = fs::symlink_metadata(&directory).ok()?;
        if !directory_metadata.is_dir() {
            return None;
        }
        let file = NamedTempFile::new_in(&directory).ok()?;
        let metadata = file.as_file().metadata().ok()?;
        #[cfg(unix)]
        if metadata.dev() != root_metadata.dev() {
            return None;
        }
        Some(Self {
            modified: metadata.modified().ok()?,
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
            #[cfg(unix)]
            device: metadata.dev(),
        })
    }

    fn proves_old_file(self, stat: FileStat) -> bool {
        let old_modified = stat
            .modified
            .is_some_and(|modified| modified < self.modified);
        #[cfg(unix)]
        let old_status = stat.status.device == self.device
            && (stat.status.changed_seconds, stat.status.changed_nanoseconds)
                < (self.changed_seconds, self.changed_nanoseconds);
        #[cfg(not(unix))]
        let old_status = true;
        old_modified && old_status
    }

    fn is_not_before(self, previous: Self) -> bool {
        #[cfg(unix)]
        let status_follows = self.device == previous.device
            && (self.changed_seconds, self.changed_nanoseconds)
                >= (previous.changed_seconds, previous.changed_nanoseconds);
        #[cfg(not(unix))]
        let status_follows = true;
        self.modified >= previous.modified && status_follows
    }
}

pub(crate) fn root_identity(root: &Path) -> Option<RootIdentity> {
    Handle::from_path(root)
        .ok()
        .map(|handle| RootIdentity(Arc::new(handle)))
}

/// Each path's stat and digests from the last request-time capture, so the next capture
/// reads and hashes only the paths whose stat moved.
///
/// A stat records length, modification and creation times, and read-only permissions.
/// Unix metadata also records status-change time, device, inode, and mode. A capture
/// reuses digests only when the stat matches and modification time precedes the filesystem
/// boundary that validated the bytes; Unix status-change time must precede it too.
/// Reuse retains that original boundary, preventing reuse within the same timestamp tick.
///
/// A record holds the paths of one capture alone, so it stays within `files_max`, and it
/// is reused only under the limits, root identity, and filesystem boundary it was captured
/// with. A failed boundary, changed root, or backwards timestamp makes the next capture
/// read every file again.
#[derive(Default)]
pub struct LastCapture {
    limits: Option<WorkspaceIndexLimits>,
    paths: HashMap<PathBuf, RecordedPath>,
    pub(crate) selection: Option<Arc<CaptureSelection>>,
    read: usize,
    boundary: Option<CaptureBoundary>,
    root_identity: Option<RootIdentity>,
}

/// One path's metadata and its validated project-relative spelling, when indexed.
struct RecordedPath {
    capture: CapturedPath,
    project: Option<Arc<ProjectPath>>,
}

/// Paths selected under one canonical root, including files the byte capture refused.
#[derive(Clone, Debug)]
pub(crate) struct CaptureSelection {
    pub(crate) root: PathBuf,
    pub(crate) source: Vec<PathBuf>,
    pub(crate) other: Vec<PathBuf>,
    pub(crate) inputs: Option<Arc<SelectionInputs>>,
    pub(crate) boundary: Option<CaptureBoundary>,
}

impl LastCapture {
    /// An empty record for one capture of up to `paths` paths under `limits`.
    pub(crate) fn under(
        limits: WorkspaceIndexLimits,
        paths: usize,
        boundary: Option<CaptureBoundary>,
        root_identity: Option<RootIdentity>,
    ) -> Self {
        Self {
            limits: Some(limits),
            paths: HashMap::with_capacity(paths),
            selection: None,
            read: 0,
            boundary,
            root_identity,
        }
    }

    pub(crate) fn selection_for(
        &self,
        limits: WorkspaceIndexLimits,
    ) -> Option<&Arc<CaptureSelection>> {
        (self.limits == Some(limits))
            .then_some(self.selection.as_ref())
            .flatten()
    }

    /// How many paths the capture that recorded this read and hashed; every other path
    /// kept its recorded digests.
    #[must_use]
    pub fn read_paths(&self) -> usize {
        self.read
    }

    /// The recorded capture of `path` when `stat` matches it under the same limits.
    fn reusable(
        &self,
        path: &Path,
        stat: FileStat,
        limits: WorkspaceIndexLimits,
        boundary: Option<CaptureBoundary>,
        root_identity: Option<&RootIdentity>,
    ) -> Option<CapturedPath> {
        let (Some(previous_boundary), Some(boundary)) = (self.boundary, boundary) else {
            return None;
        };
        if self.limits != Some(limits)
            || stat.modified.is_none()
            || self.root_identity.as_ref() != root_identity
            || self.root_identity.is_none()
            || !boundary.is_not_before(previous_boundary)
        {
            return None;
        }
        self.paths
            .get(path)
            .map(|recorded| &recorded.capture)
            .filter(|captured| {
                captured.stat == stat
                    && captured
                        .boundary
                        .is_some_and(|proof| proof.proves_old_file(stat))
            })
            .copied()
    }

    pub(crate) fn captured(&self, path: &Path) -> Option<CapturedPath> {
        self.paths.get(path).map(|recorded| recorded.capture)
    }

    pub(crate) fn project_path(&self, root: &Path, path: &Path) -> Option<&Arc<ProjectPath>> {
        self.selection
            .as_ref()
            .filter(|selection| selection.root == root)?;
        self.paths.get(path)?.project.as_ref()
    }

    /// Caches validation only for a path whose metadata this capture already recorded.
    pub(crate) fn remember_project_path(&mut self, path: &Path, project: Arc<ProjectPath>) {
        if let Some(recorded) = self.paths.get_mut(path) {
            recorded.project = Some(project);
        }
    }

    pub(crate) const fn boundary(&self) -> Option<CaptureBoundary> {
        self.boundary
    }

    pub(crate) fn root_identity(&self) -> Option<RootIdentity> {
        self.root_identity.clone()
    }

    /// Records one captured path, counting it when it was read.
    pub(crate) fn record(&mut self, path: &Path, captured: CapturedPath, was_read: bool) {
        self.record_projected(path, captured, was_read, None);
    }

    pub(crate) fn record_projected(
        &mut self,
        path: &Path,
        captured: CapturedPath,
        was_read: bool,
        project: Option<Arc<ProjectPath>>,
    ) {
        self.paths.insert(
            path.to_path_buf(),
            RecordedPath {
                capture: captured,
                project,
            },
        );
        self.read += usize::from(was_read);
    }
}

impl std::fmt::Debug for LastCapture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LastCapture")
            .field("paths", &self.paths.len())
            .field("read", &self.read)
            .finish_non_exhaustive()
    }
}

/// Captures `path`: the digests `last` recorded when its stat did not move, otherwise a
/// fresh read. The flag says whether the path was read.
pub(crate) fn capture_path(
    path: &Path,
    limits: WorkspaceIndexLimits,
    last: &LastCapture,
    boundary: Option<CaptureBoundary>,
    root_identity: Option<&RootIdentity>,
) -> Result<(CapturedPath, bool), RiftError> {
    capture_path_with(path, limits, last, boundary, root_identity, || {})
}

fn capture_path_with(
    path: &Path,
    limits: WorkspaceIndexLimits,
    last: &LastCapture,
    boundary: Option<CaptureBoundary>,
    root_identity: Option<&RootIdentity>,
    before_open: impl FnOnce(),
) -> Result<(CapturedPath, bool), RiftError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| capture_io_error(path, error))?;
    if !metadata.is_file() {
        return errors::index::workspace_changed_during_capture()
            .path(path)
            .fail();
    }
    let stat = FileStat::of(&metadata);
    if let Some(captured) = last.reusable(path, stat, limits, boundary, root_identity) {
        return Ok((captured, false));
    }
    before_open();
    CapturedFile::read(path, limits, boundary).map(|captured| (captured, true))
}

/// One path's stat, verified raw content, and optional indexed file states.
#[derive(Clone, Copy)]
pub(crate) struct CapturedPath {
    stat: FileStat,
    file: Option<CapturedFile>,
    content: Option<(usize, FileDigest)>,
    boundary: Option<CaptureBoundary>,
}

impl CapturedPath {
    /// Verified raw bytes within the per-file bound, including files syntax refuses.
    pub(crate) const fn content(&self) -> Option<(usize, FileDigest)> {
        self.content
    }

    /// The captured digests, `None` for a file the index leaves out.
    pub(crate) const fn file(&self) -> Option<CapturedFile> {
        self.file
    }
}

/// The file metadata a capture compares before it reads a path again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStat {
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    status: UnixStatus,
}

/// The unix stat fields a rewrite moves while it keeps the length and restores the
/// modification time.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UnixStatus {
    device: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    inode: u64,
    mode: u32,
}

impl FileStat {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            status: UnixStatus::of(metadata),
        }
    }
}

#[cfg(unix)]
impl UnixStatus {
    fn of(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;

        Self {
            device: metadata.dev(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            inode: metadata.ino(),
            mode: metadata.mode(),
        }
    }
}

/// One file's captured digests, and the byte length it counts against the workspace.
#[derive(Clone, Copy)]
pub(crate) struct CapturedFile {
    pub(crate) length: usize,
    pub(crate) state: FileDigest,
    pub(crate) content: FileDigest,
}

impl CapturedFile {
    /// Reads and hashes the file at `path` under the stat its open handle reports first.
    /// The file is absent when the index leaves it out: past `file_bytes_max`, holding a
    /// NUL byte, or not UTF-8.
    fn read(
        path: &Path,
        limits: WorkspaceIndexLimits,
        boundary: Option<CaptureBoundary>,
    ) -> Result<CapturedPath, RiftError> {
        Self::read_with(path, limits, boundary, || {})
    }

    /// Reads `path`, then runs `after_read` before checking that path still names the open file.
    fn read_with(
        path: &Path,
        limits: WorkspaceIndexLimits,
        boundary: Option<CaptureBoundary>,
        after_read: impl FnOnce(),
    ) -> Result<CapturedPath, RiftError> {
        let handle = fs::File::open(path).map_err(|error| capture_io_error(path, error))?;
        let metadata = handle.metadata().map_err(|error| {
            errors::index::workspace_filesystem()
                .path(path)
                .source(error)
                .error()
        })?;
        let stat = FileStat::of(&metadata);
        #[cfg(not(unix))]
        let held_identity = Handle::from_file(handle.try_clone().map_err(|error| {
            errors::index::workspace_filesystem()
                .path(path)
                .source(error)
                .error()
        })?)
        .map_err(|error| {
            errors::index::workspace_filesystem()
                .path(path)
                .source(error)
                .error()
        })?;
        let bytes = read_file_bytes(handle, path, limits)?;
        after_read();
        #[cfg(not(unix))]
        {
            let named_identity =
                Handle::from_path(path).map_err(|error| capture_io_error(path, error))?;
            if held_identity != named_identity {
                return errors::index::workspace_changed_during_capture()
                    .path(path)
                    .fail();
            }
        }
        let after_path = fs::metadata(path).map_err(|error| capture_io_error(path, error))?;
        if FileStat::of(&after_path) != stat {
            return errors::index::workspace_changed_during_capture()
                .path(path)
                .fail();
        }
        if bytes.len() > limits.file_bytes_max() {
            return Ok(CapturedPath {
                stat,
                file: None,
                content: None,
                boundary: boundary.filter(|boundary| boundary.proves_old_file(stat)),
            });
        }
        if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
            return Ok(CapturedPath {
                stat,
                file: None,
                content: Some((bytes.len(), FileDigest::of(&bytes))),
                boundary: boundary.filter(|boundary| boundary.proves_old_file(stat)),
            });
        }
        let (content, state) =
            FileDigest::of_content_and_file_state(&bytes, metadata_is_executable(&metadata));
        Ok(CapturedPath {
            stat,
            file: Some(Self {
                length: bytes.len(),
                state,
                content,
            }),
            content: Some((bytes.len(), content)),
            boundary: boundary.filter(|boundary| boundary.proves_old_file(stat)),
        })
    }
}

fn capture_io_error(path: &Path, source: std::io::Error) -> RiftError {
    if source.kind() == std::io::ErrorKind::NotFound {
        errors::index::workspace_changed_during_capture()
            .path(path)
            .source(source)
            .error()
    } else {
        errors::index::workspace_filesystem()
            .path(path)
            .source(source)
            .error()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{LastCapture, capture_path_with};
    use crate::workspace::WorkspaceIndexLimits;
    use rift_error::errors;

    #[test]
    fn test_capture_refuses_path_replaced_after_read() {
        use std::fs::OpenOptions;

        use super::CapturedFile;

        let directory = tempfile::tempdir().expect("workspace");
        let path = directory.path().join("source.rs");
        let replacement = directory.path().join("replacement.rs");
        let modified = std::time::SystemTime::now();
        fs::write(&path, b"pub fn old() {}\n").expect("original source");
        fs::write(&replacement, b"pub fn new() {}\n").expect("replacement source");
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("original handle")
            .set_modified(modified)
            .expect("set original time");
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&replacement)
            .expect("replacement handle")
            .set_modified(modified)
            .expect("set replacement time");
        let original_metadata = fs::metadata(&path).expect("original stat");
        let replacement_metadata = fs::metadata(&replacement).expect("replacement stat");
        assert_eq!(original_metadata.len(), replacement_metadata.len());
        assert_eq!(
            original_metadata.modified().expect("original mtime"),
            replacement_metadata.modified().expect("replacement mtime")
        );

        let result = CapturedFile::read_with(&path, WorkspaceIndexLimits::default(), None, || {
            fs::remove_file(&path).expect("remove captured path");
            fs::rename(&replacement, &path).expect("install replacement at captured path");
        });
        let Err(error) = result else {
            panic!("path identity change invalidates capture");
        };

        assert_eq!(
            error.slug(),
            errors::index::workspace_changed_during_capture::SLUG
        );
    }

    #[test]
    fn test_capture_classifies_a_missing_open_path_as_changed() {
        let directory = tempfile::tempdir().expect("workspace");
        let path = directory.path().join("source.rs");
        fs::write(&path, b"pub fn source() {}\n").expect("source file");
        let result = capture_path_with(
            &path,
            WorkspaceIndexLimits::default(),
            &LastCapture::default(),
            None,
            None,
            || fs::remove_file(&path).expect("remove after stat, before open"),
        );
        let Err(error) = result else {
            panic!("removed path fails the capture");
        };
        assert_eq!(
            error.slug(),
            errors::index::workspace_changed_during_capture::SLUG
        );
        assert!(
            std::error::Error::source(&error).is_some(),
            "missing path keeps original operating-system cause"
        );
    }
}
