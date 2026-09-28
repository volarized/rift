//! The live and fill locks beside one store file.
//!
//! `flock` is advisory, so the operating system never blocks an unlink: a
//! lock file a sweeper deletes while a server holds it leaves that server
//! holding a lock on a file no other opener can reach. Every opener therefore
//! checks after locking that the path still names the file it locked, and
//! opens the path again when it does not.

use std::fs::{File, OpenOptions};
use std::path::Path;

use rift_core::Error;

use crate::error::{StoreError, StoreFault, folder_error};

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
/// Returns [`StoreError`] when the lock file cannot be opened or locked, or
/// when [`LIVE_LOCK_ATTEMPTS_MAX`] attempts each locked a file the path no
/// longer named.
pub(crate) fn lock_live(path: &Path) -> Result<File, StoreError> {
    lock_live_checked(path, &mut |_| {})
}

/// [`lock_live`] with `after_lock` run between each lock and the check that
/// the path still names the locked file, so a test replaces the file inside
/// that window deterministically.
pub(crate) fn lock_live_checked(
    path: &Path,
    after_lock: &mut dyn FnMut(usize),
) -> Result<File, StoreError> {
    for attempt in 0..LIVE_LOCK_ATTEMPTS_MAX {
        let file = open_lock(path).map_err(folder_error(path, "open live lock"))?;
        file.lock_shared()
            .map_err(folder_error(path, "lock live store"))?;
        after_lock(attempt);
        if names_file(path, &file) {
            return Ok(file);
        }
    }
    Err(Error::new(StoreFault::LockUnstable {
        path: path.to_owned(),
        attempts: LIVE_LOCK_ATTEMPTS_MAX,
    }))
}

/// Whether `path` still names `file`: the same device and inode.
#[cfg(unix)]
fn names_file(path: &Path, file: &File) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(held), Ok(named)) => held.dev() == named.dev() && held.ino() == named.ino(),
        _ => false,
    }
}

/// Whether `path` still names `file`. Stable std exposes no file identity
/// off Unix, so the path existing is the whole check there: a replaced file
/// passes it.
#[cfg(not(unix))]
fn names_file(path: &Path, _file: &File) -> bool {
    path.exists()
}
