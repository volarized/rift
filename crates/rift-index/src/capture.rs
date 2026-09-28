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

use crate::change_set::FileDigest;
use crate::workspace::{
    WorkspaceIndexError, WorkspaceIndexLimits, WorkspaceIndexViolation, index_error_caused_by,
    metadata_is_executable, read_file_bytes,
};

/// Each path's stat and digests from the last request-time capture, so the next capture
/// reads and hashes only the paths whose stat moved.
///
/// A stat is the file's length and modification time, and on unix also its status change
/// time, inode, and mode. A capture that reads a file takes the stat from the open handle
/// before the bytes, so a write that lands during the read moves the next stat. Restoring
/// a modification time (`touch -r`, `cp -p`, `rsync -t`) moves the status change time, so
/// such a rewrite is read again.
///
/// One miss stays: a rewrite to bytes of the same length that lands inside the same
/// timestamp tick as the recorded stat keeps the recorded digest until the file's stat
/// moves again. Filesystems that stamp nanoseconds, such as APFS and ext4, make that tick
/// too short to matter; those with 1 s or 2 s ticks, such as HFS+, FAT, and some network
/// mounts, widen it.
///
/// A record holds the paths of one capture alone, so it stays within `files_max`, and it
/// is reused only under the limits it was captured with. Two captures that run at once
/// each record what they read; whichever is kept, every recorded digest matched its stat
/// when it was read, so reusing an older record costs reads, never a wrong digest.
#[derive(Default)]
pub struct LastCapture {
    limits: Option<WorkspaceIndexLimits>,
    paths: HashMap<PathBuf, CapturedPath>,
    read: usize,
}

impl LastCapture {
    /// An empty record for one capture of up to `paths` paths under `limits`.
    pub(crate) fn under(limits: WorkspaceIndexLimits, paths: usize) -> Self {
        Self {
            limits: Some(limits),
            paths: HashMap::with_capacity(paths),
            read: 0,
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
    ) -> Option<CapturedPath> {
        if self.limits != Some(limits) || stat.modified.is_none() {
            return None;
        }
        self.paths
            .get(path)
            .filter(|captured| captured.stat == stat)
            .copied()
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
) -> Result<(CapturedPath, bool), WorkspaceIndexError> {
    let metadata = fs::metadata(path).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    if let Some(captured) = last.reusable(path, FileStat::of(&metadata), limits) {
        return Ok((captured, false));
    }
    CapturedFile::read(path, limits).map(|captured| (captured, true))
}

/// One path's stat and its captured digests, absent for a file the index leaves out.
#[derive(Clone, Copy)]
pub(crate) struct CapturedPath {
    stat: FileStat,
    file: Option<CapturedFile>,
}

impl CapturedPath {
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
    ) -> Result<CapturedPath, WorkspaceIndexError> {
        let handle = fs::File::open(path).map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        let metadata = handle.metadata().map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        let stat = FileStat::of(&metadata);
        let bytes = read_file_bytes(handle, path, limits)?;
        if bytes.len() > limits.file_bytes_max()
            || bytes.contains(&0)
            || std::str::from_utf8(&bytes).is_err()
        {
            return Ok(CapturedPath { stat, file: None });
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
        })
    }
}
