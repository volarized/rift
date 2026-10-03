//! Stat-keyed reuse of request-time capture digests.
//!
//! Index validation captures the workspace on every request, and the capture is the
//! content comparison: a read accepts a publication only when the captured digests fold
//! to its fingerprint. Reading and hashing every file on every request costs the whole
//! tree's bytes, so each capture records every path's stat beside the digests it
//! captured, and the next capture reads and hashes only the paths whose stat moved.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
use rift_core::constants::RIFT_STATE_DIRECTORY;
#[cfg(unix)]
use tempfile::NamedTempFile;

use crate::change_set::FileDigest;
use crate::workspace::{
    WorkspaceIndexError, WorkspaceIndexLimits, WorkspaceIndexViolation, index_error_caused_by,
    metadata_is_executable, read_file_bytes,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::sys::statfs::statfs;
#[cfg(not(unix))]
use same_file::Handle;

#[cfg(target_os = "linux")]
fn supports_stat_reuse_type(filesystem_type: nix::sys::statfs::FsType) -> bool {
    use nix::sys::statfs::{EXT4_SUPER_MAGIC, TMPFS_MAGIC};

    matches!(filesystem_type, EXT4_SUPER_MAGIC | TMPFS_MAGIC)
}

#[cfg(target_os = "macos")]
fn supports_stat_reuse_type(filesystem_type_name: &str) -> bool {
    matches!(filesystem_type_name, "apfs" | "hfs")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn supports_stat_reuse(root: &Path) -> bool {
    statfs(root).is_ok_and(|filesystem| {
        #[cfg(target_os = "linux")]
        {
            supports_stat_reuse_type(filesystem.filesystem_type())
        }
        #[cfg(target_os = "macos")]
        {
            supports_stat_reuse_type(filesystem.filesystem_type_name())
        }
    })
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn supports_stat_reuse(_root: &Path) -> bool {
    false
}

#[cfg(all(not(unix), test))]
pub(crate) fn supports_stat_reuse(_root: &Path) -> bool {
    false
}

/// A timestamped file on the workspace filesystem, created before capture reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureBoundary {
    #[cfg(unix)]
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
        {
            use std::os::unix::fs::MetadataExt as _;

            if !supports_stat_reuse(root) {
                return None;
            }
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
            if !directory_metadata.is_dir() || directory_metadata.dev() != root_metadata.dev() {
                return None;
            }
            let file = NamedTempFile::new_in(&directory).ok()?;
            let metadata = file.as_file().metadata().ok()?;
            if metadata.dev() != root_metadata.dev() {
                return None;
            }
            let modified = metadata.modified().ok()?;
            Some(Self {
                modified,
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
                device: metadata.dev(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = root;
            None
        }
    }

    #[cfg(unix)]
    fn proves_old_file(self, stat: FileStat) -> bool {
        stat.modified
            .is_some_and(|modified| modified < self.modified)
            && stat.status.device == self.device
            && (stat.status.changed_seconds, stat.status.changed_nanoseconds)
                < (self.changed_seconds, self.changed_nanoseconds)
    }

    #[cfg(unix)]
    fn is_not_before(self, previous: Self) -> bool {
        self.device == previous.device
            && self.modified >= previous.modified
            && (self.changed_seconds, self.changed_nanoseconds)
                >= (previous.changed_seconds, previous.changed_nanoseconds)
    }

    #[cfg(not(unix))]
    fn proves_old_file(self, _stat: FileStat) -> bool {
        false
    }

    #[cfg(not(unix))]
    fn is_not_before(self, _previous: Self) -> bool {
        false
    }
}

#[cfg(unix)]
pub(crate) fn root_identity(root: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = fs::metadata(root).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
pub(crate) fn root_identity(_root: &Path) -> Option<(u64, u64)> {
    None
}

/// Each path's stat and digests from the last request-time capture, so the next capture
/// reads and hashes only the paths whose stat moved.
///
/// A stat is the file's length and modification time, and on unix also its status change
/// time, device, inode, and mode. A capture reuses a digest only when its stat still
/// matches and both timestamps precede the boundary that validated the bytes. Reuse keeps
/// that original boundary; it does not renew it. Only selected filesystem types reuse
/// digests; every other type reads every file again.
///
/// A record holds the paths of one capture alone, so it stays within `files_max`, and it
/// is reused only under the limits, root identity, and filesystem boundary it was captured
/// with. A failed boundary, changed root, or backwards timestamp makes the next capture
/// read every file again.
#[derive(Default)]
pub struct LastCapture {
    limits: Option<WorkspaceIndexLimits>,
    paths: HashMap<PathBuf, CapturedPath>,
    read: usize,
    boundary: Option<CaptureBoundary>,
    root_identity: Option<(u64, u64)>,
}

impl LastCapture {
    /// An empty record for one capture of up to `paths` paths under `limits`.
    pub(crate) fn under(
        limits: WorkspaceIndexLimits,
        paths: usize,
        boundary: Option<CaptureBoundary>,
        root_identity: Option<(u64, u64)>,
    ) -> Self {
        Self {
            limits: Some(limits),
            paths: HashMap::with_capacity(paths),
            read: 0,
            boundary,
            root_identity,
        }
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
        root_identity: Option<(u64, u64)>,
    ) -> Option<CapturedPath> {
        let (Some(previous_boundary), Some(boundary)) = (self.boundary, boundary) else {
            return None;
        };
        if self.limits != Some(limits)
            || stat.modified.is_none()
            || self.root_identity != root_identity
            || self.root_identity.is_none()
            || !boundary.is_not_before(previous_boundary)
        {
            return None;
        }
        self.paths
            .get(path)
            .filter(|captured| {
                captured.stat == stat
                    && captured
                        .boundary
                        .is_some_and(|proof| proof.proves_old_file(stat))
            })
            .copied()
    }

    pub(crate) fn captured(&self, path: &Path) -> Option<CapturedPath> {
        self.paths.get(path).copied()
    }

    pub(crate) const fn boundary(&self) -> Option<CaptureBoundary> {
        self.boundary
    }

    pub(crate) const fn root_identity(&self) -> Option<(u64, u64)> {
        self.root_identity
    }

    /// Records one captured path, counting it when it was read.
    pub(crate) fn record(&mut self, path: &Path, captured: CapturedPath, was_read: bool) {
        self.paths.insert(path.to_path_buf(), captured);
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
    root_identity: Option<(u64, u64)>,
) -> Result<(CapturedPath, bool), WorkspaceIndexError> {
    capture_path_with(path, limits, last, boundary, root_identity, || {})
}

fn capture_path_with(
    path: &Path,
    limits: WorkspaceIndexLimits,
    last: &LastCapture,
    boundary: Option<CaptureBoundary>,
    root_identity: Option<(u64, u64)>,
    before_open: impl FnOnce(),
) -> Result<(CapturedPath, bool), WorkspaceIndexError> {
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            index_error_caused_by(
                WorkspaceIndexViolation::ChangedDuringCapture,
                Some(path),
                error,
            )
        } else {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        }
    })?;
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
    ) -> Result<CapturedPath, WorkspaceIndexError> {
        Self::read_with(path, limits, boundary, || {})
    }

    /// Reads `path`, then runs `after_read` before checking that path still names the open file.
    fn read_with(
        path: &Path,
        limits: WorkspaceIndexLimits,
        boundary: Option<CaptureBoundary>,
        after_read: impl FnOnce(),
    ) -> Result<CapturedPath, WorkspaceIndexError> {
        let handle = fs::File::open(path).map_err(|error| {
            let violation = if error.kind() == std::io::ErrorKind::NotFound {
                WorkspaceIndexViolation::ChangedDuringCapture
            } else {
                WorkspaceIndexViolation::Filesystem
            };
            index_error_caused_by(violation, Some(path), error)
        })?;
        let metadata = handle.metadata().map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        let stat = FileStat::of(&metadata);
        #[cfg(not(unix))]
        let held_identity = Handle::from_file(handle.try_clone().map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?)
        .map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        let bytes = read_file_bytes(handle, path, limits)?;
        after_read();
        #[cfg(not(unix))]
        {
            let named_identity = Handle::from_path(path).map_err(|error| {
                let violation = if error.kind() == std::io::ErrorKind::NotFound {
                    WorkspaceIndexViolation::ChangedDuringCapture
                } else {
                    WorkspaceIndexViolation::Filesystem
                };
                index_error_caused_by(violation, Some(path), error)
            })?;
            if held_identity != named_identity {
                return Err(crate::workspace::index_error_at(
                    WorkspaceIndexViolation::ChangedDuringCapture,
                    path,
                ));
            }
        }
        let after_path = fs::metadata(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                index_error_caused_by(
                    WorkspaceIndexViolation::ChangedDuringCapture,
                    Some(path),
                    error,
                )
            } else {
                index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
            }
        })?;
        if FileStat::of(&after_path) != stat {
            return Err(crate::workspace::index_error_at(
                WorkspaceIndexViolation::ChangedDuringCapture,
                path,
            ));
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{LastCapture, capture_path_with};
    use crate::workspace::{WorkspaceIndexLimits, WorkspaceIndexViolation};

    #[cfg(target_os = "linux")]
    #[test]
    fn test_stat_reuse_filesystem_allowlist_is_closed() {
        use nix::sys::statfs::{EXT4_SUPER_MAGIC, OVERLAYFS_SUPER_MAGIC, TMPFS_MAGIC};

        assert!(super::supports_stat_reuse_type(EXT4_SUPER_MAGIC));
        assert!(super::supports_stat_reuse_type(TMPFS_MAGIC));
        assert!(!super::supports_stat_reuse_type(OVERLAYFS_SUPER_MAGIC));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_stat_reuse_filesystem_allowlist_is_closed() {
        for supported in ["apfs", "hfs"] {
            assert!(super::supports_stat_reuse_type(supported));
        }
        for unsupported in ["msdos", "exfat", "nfs", "unknown"] {
            assert!(!super::supports_stat_reuse_type(unsupported));
        }
    }

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
            error.fault().violation(),
            WorkspaceIndexViolation::ChangedDuringCapture
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
            error.fault().violation(),
            WorkspaceIndexViolation::ChangedDuringCapture
        );
        assert!(
            !rift_core::causes(&error).is_empty(),
            "missing path keeps original operating-system cause"
        );
    }
}
