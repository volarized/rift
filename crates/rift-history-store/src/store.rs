//! Where a store lives, how a server opens it, and how stale revisions leave.

use std::fs::{File, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rift_core::constants::RIFT_STATE_DIRECTORY;
use rift_error::{RiftError, errors};

use crate::database::{StoreFiller, StoreReader, create_schema};
use crate::lock::{lock_live, open_lock};

/// The folder inside the common git directory that holds every store file:
/// `.rift`, the spelling of every Rift state directory.
pub const STORE_FOLDER_NAME: &str = RIFT_STATE_DIRECTORY;

/// The file-name prefix every store file of one repository shares.
const STORE_FILE_PREFIX: &str = "store-";
/// The suffix of the database file one derivation revision fills.
const DATABASE_SUFFIX: &str = ".db";
/// The suffixes `SQLite` gives a WAL database's companion files.
const DATABASE_COMPANION_SUFFIXES: [&str; 2] = [".db-wal", ".db-shm"];
/// The suffix of the lock every live server holds shared.
const LIVE_LOCK_SUFFIX: &str = ".live.lock";
/// The suffix of the lock the one filler holds exclusively.
const FILL_LOCK_SUFFIX: &str = ".fill.lock";

/// Where one derivation revision's store lives: the `.rift/` folder of the
/// common git directory, with the worktree's state directory as the folder a
/// refusal falls back to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreLocation {
    folder: PathBuf,
    fallback: Option<PathBuf>,
    revision: String,
}

impl StoreLocation {
    /// The store for `revision` inside `<common_git_directory>/.rift/`.
    #[must_use]
    pub fn new(common_git_directory: &Path, revision: &str) -> Self {
        Self {
            folder: common_git_directory.join(STORE_FOLDER_NAME),
            fallback: None,
            revision: revision.to_owned(),
        }
    }

    /// The same store, kept in `worktree_state`, the worktree's own state
    /// directory, when the common git directory refuses its folder for want of
    /// write access.
    #[must_use]
    pub fn or_worktree(mut self, worktree_state: &Path) -> Self {
        self.fallback = Some(worktree_state.to_owned());
        self
    }

    /// The folder the store's files sit in.
    #[must_use]
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// The derivation revision this store's rows were derived under.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// This store in the fallback folder, when `refused` is the common git
    /// directory refusing the folder for want of write access.
    fn in_worktree(&self, refused: &RiftError) -> Option<Self> {
        let denied = std::error::Error::source(refused)
            .and_then(|source| source.downcast_ref::<std::io::Error>())
            .is_some_and(|cause| {
                matches!(
                    cause.kind(),
                    ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
                )
            });
        let fallback = self.fallback.as_ref().filter(|_| denied)?;
        Some(Self {
            folder: fallback.clone(),
            fallback: None,
            revision: self.revision.clone(),
        })
    }

    /// The path of `revision`'s file carrying `suffix`.
    fn file(&self, revision: &str, suffix: &str) -> PathBuf {
        self.folder
            .join(format!("{STORE_FILE_PREFIX}{revision}{suffix}"))
    }

    /// The database file this location's revision fills.
    pub(crate) fn database(&self) -> PathBuf {
        self.file(&self.revision, DATABASE_SUFFIX)
    }
}

/// The refusal that moved a store out of the common git directory and into
/// the worktree's state directory. Sharing across worktrees is lost for this
/// repository while it holds.
#[derive(Debug)]
pub struct WorktreeFallback {
    refused: PathBuf,
    cause: RiftError,
}

impl WorktreeFallback {
    /// The folder of the common git directory that was refused.
    #[must_use]
    pub fn refused(&self) -> &Path {
        &self.refused
    }

    /// Why it was refused.
    #[must_use]
    pub const fn cause(&self) -> &RiftError {
        &self.cause
    }
}

/// What one sweep removed, and what it could not remove.
#[derive(Debug, Default)]
pub struct SweptRevisions {
    deleted: Vec<String>,
    failures: Vec<RiftError>,
}

impl SweptRevisions {
    /// The derivation revisions whose files the sweep deleted, sorted.
    #[must_use]
    pub fn deleted(&self) -> &[String] {
        &self.deleted
    }

    /// The released revisions the sweep could not take: a live lock it could
    /// not open or try, and a deletion the filesystem refused.
    #[must_use]
    pub fn failures(&self) -> &[RiftError] {
        &self.failures
    }
}

/// One server's open store: its resolved location and the live lock it holds
/// for as long as the server runs.
#[derive(Debug)]
pub struct HistoryStore {
    location: StoreLocation,
    fallback: Option<WorktreeFallback>,
    _live: File,
}

impl HistoryStore {
    /// Opens or creates the store and takes its live lock shared, which keeps
    /// every sweep off its files while this handle lives. When the common git
    /// directory refuses the folder for want of write access and `location`
    /// names a fallback, the store opens there and
    /// [`Self::worktree_fallback`] says so.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when neither folder takes the store, the live
    /// lock cannot be held, or `SQLite` refuses the file.
    pub fn open(location: &StoreLocation) -> Result<Self, RiftError> {
        match Self::open_in(location) {
            Err(refused) => {
                let Some(fallback) = location.in_worktree(&refused) else {
                    return refused.fail();
                };
                let mut store = Self::open_in(&fallback)?;
                store.fallback = Some(WorktreeFallback {
                    refused: location.folder.clone(),
                    cause: refused,
                });
                Ok(store)
            }
            opened => opened,
        }
    }

    fn open_in(location: &StoreLocation) -> Result<Self, RiftError> {
        std::fs::create_dir_all(&location.folder).map_err(|source| {
            errors::history_store::folder()
                .operation("create store folder")
                .path(&location.folder)
                .detail(source)
                .error()
        })?;
        let live = lock_live(&location.file(&location.revision, LIVE_LOCK_SUFFIX))?;
        create_schema(&location.database())?;
        Ok(Self {
            location: location.clone(),
            fallback: None,
            _live: live,
        })
    }

    /// Where the store opened: the common git directory's folder, or the
    /// fallback folder after a refusal.
    #[must_use]
    pub const fn location(&self) -> &StoreLocation {
        &self.location
    }

    /// The refusal that moved the store into the worktree's state directory,
    /// if one did.
    #[must_use]
    pub const fn worktree_fallback(&self) -> Option<&WorktreeFallback> {
        self.fallback.as_ref()
    }

    /// Opens read connections to this store.
    #[must_use]
    pub fn reader(&self) -> StoreReader {
        StoreReader::new(self.location.database())
    }

    /// Takes the fill lock and a write connection, or `None` while another
    /// server's history task fills this store.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the fill lock cannot be opened or tried, or
    /// `SQLite` refuses the write connection.
    pub fn filler(&self) -> Result<Option<StoreFiller>, RiftError> {
        let path = self
            .location
            .file(&self.location.revision, FILL_LOCK_SUFFIX);
        let lock = open_lock(&path).map_err(|source| {
            errors::history_store::folder()
                .operation("open fill lock")
                .path(&path)
                .detail(source)
                .error()
        })?;
        match lock.try_lock() {
            Ok(()) => StoreFiller::open(&self.location.database(), lock).map(Some),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(source)) => errors::history_store::folder()
                .operation("lock fill")
                .path(&path)
                .detail(source)
                .fail(),
        }
    }

    /// Deletes the files of every other derivation revision whose live lock no
    /// server holds: a server of that build holds it shared for as long as it
    /// runs. A revision's files go only while this sweep holds its live lock
    /// exclusively, and the live lock file goes last, so a server opening that
    /// revision meanwhile finds its path moved and locks the new file.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the folder cannot be listed. A live lock the
    /// sweep cannot open or try, and a file the filesystem refuses to delete,
    /// are reported in [`SweptRevisions::failures`] and leave the rest of the
    /// sweep running.
    pub fn sweep(&self) -> Result<SweptRevisions, RiftError> {
        let folder = &self.location.folder;
        let entries = std::fs::read_dir(folder).map_err(|source| {
            errors::history_store::folder()
                .operation("list store folder")
                .path(folder)
                .detail(source)
                .error()
        })?;
        let mut swept = SweptRevisions::default();
        for entry in entries {
            let entry = entry.map_err(|source| {
                errors::history_store::folder()
                    .operation("list store folder")
                    .path(folder)
                    .detail(source)
                    .error()
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(revision) = name
                .strip_prefix(STORE_FILE_PREFIX)
                .and_then(|rest| rest.strip_suffix(LIVE_LOCK_SUFFIX))
            else {
                continue;
            };
            if revision == self.location.revision {
                continue;
            }
            self.sweep_revision(revision, &entry.path(), &mut swept);
        }
        swept.deleted.sort();
        Ok(swept)
    }

    /// Deletes one released revision's files, its live lock last, while
    /// holding that lock exclusively. A lock another server holds is skipped;
    /// a lock the sweep cannot open or try is recorded in `swept.failures`.
    fn sweep_revision(&self, revision: &str, live: &Path, swept: &mut SweptRevisions) {
        let lock = match lock_released(live) {
            Ok(Some(lock)) => lock,
            Ok(None) => return,
            Err(failure) => {
                swept.failures.push(failure);
                return;
            }
        };
        let mut files: Vec<PathBuf> = std::iter::once(DATABASE_SUFFIX)
            .chain(DATABASE_COMPANION_SUFFIXES)
            .chain([FILL_LOCK_SUFFIX])
            .map(|suffix| self.location.file(revision, suffix))
            .collect();
        files.push(live.to_owned());
        for file in files {
            if let Err(failure) = remove_released(&file) {
                // The live lock stays while any other file of the revision
                // does, so a later sweep finds the revision again.
                swept.failures.push(failure);
                return;
            }
        }
        drop(lock);
        swept.deleted.push(revision.to_owned());
    }
}

/// Takes a revision's live lock at `live` exclusively; `None` while a server
/// holds it shared.
///
/// std's `try_lock` separates the two outcomes: `TryLockError::WouldBlock`
/// when the lock "is held by another handle/process", and
/// `TryLockError::Error` for "an I/O error on the file", which never carries
/// `ErrorKind::WouldBlock`.
fn lock_released(live: &Path) -> Result<Option<File>, RiftError> {
    let lock = open_lock(live).map_err(|source| {
        errors::history_store::folder()
            .operation("open swept live lock")
            .path(live)
            .detail(source)
            .error()
    })?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(source)) => errors::history_store::folder()
            .operation("lock swept live lock")
            .path(live)
            .detail(source)
            .fail(),
    }
}

/// Deletes one file of a released revision; an absent file is already gone.
fn remove_released(file: &Path) -> Result<(), RiftError> {
    match std::fs::remove_file(file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => errors::history_store::folder()
            .operation("delete swept store file")
            .path(file)
            .detail(source)
            .fail(),
    }
}
