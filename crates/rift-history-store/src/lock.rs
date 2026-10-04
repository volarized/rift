//! The live and fill locks beside one store file.
//!
//! A lock never blocks an unlink: `flock` is advisory, and on Windows every
//! handle the standard library opens shares delete access, so a delete succeeds
//! while another process holds the file open. A lock file a sweeper deletes
//! while a server holds it leaves that server holding a lock on a file no other
//! opener can reach.
//! Every opener therefore checks after locking that the path still names the
//! file it locked, and opens the path again when it does not.

use std::fs::{File, OpenOptions};
use std::path::Path;

use rift_error::{RiftError, errors};
use same_file::Handle;

/// Attempts one opener makes to lock a live lock a sweeper keeps replacing.
pub(crate) const LIVE_LOCK_ATTEMPTS_MAX: usize = 3;

/// Opens a lock file, creating it when absent. The file's bytes are never
/// read: on Windows an exclusive lock blocks other processes' reads of the
/// locked file, so a lock lives on a companion file nobody reads.
pub(crate) fn open_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

/// Takes the live lock at `path` shared, opening the path again when a sweep
/// replaced the file between the open and the lock.
///
/// # Errors
///
/// Returns [`RiftError`] when the lock file cannot be opened or locked, or
/// when [`LIVE_LOCK_ATTEMPTS_MAX`] attempts each locked a file the path no
/// longer named.
pub(crate) fn lock_live(path: &Path) -> Result<File, RiftError> {
    lock_live_checked(path, &mut |_| {})
}

/// [`lock_live`] with `after_lock` run between each lock and the check that
/// the path still names the locked file, so a test replaces the file inside
/// that window deterministically.
pub(crate) fn lock_live_checked(
    path: &Path,
    after_lock: &mut dyn FnMut(usize),
) -> Result<File, RiftError> {
    for attempt in 0..LIVE_LOCK_ATTEMPTS_MAX {
        let file = open_lock(path).map_err(|source| {
            errors::history_store::folder()
                .operation("open live lock")
                .path(path)
                .detail(source)
                .error()
        })?;
        file.lock_shared().map_err(|source| {
            errors::history_store::folder()
                .operation("lock live store")
                .path(path)
                .detail(source)
                .error()
        })?;
        after_lock(attempt);
        if names_file(path, &file) {
            return Ok(file);
        }
    }
    errors::history_store::lock_unstable()
        .path(path)
        .attempts(LIVE_LOCK_ATTEMPTS_MAX)
        .fail()
}

/// Whether `path` still names `file`: the same file by the identity the
/// platform keeps, the device and inode on Unix, the volume serial number and
/// file index on Windows.
///
/// Stable std exposes no file identity on Windows, and `same-file` reads it
/// through `GetFileInformationByHandle` while both handles stay open, as the
/// comparison requires. A path that names no file, or a file either side cannot
/// read an identity from, names nothing this opener holds.
fn names_file(path: &Path, file: &File) -> bool {
    let held = file.try_clone().and_then(Handle::from_file);
    let named = Handle::from_path(path);
    matches!((held, named), (Ok(held), Ok(named)) if held == named)
}
