//! The history store: the commits the history task analyzed, kept once per
//! repository so every worktree of it reads them without analyzing them again.
//!
//! The store is a set of `SQLite` tables filled from the revisions the
//! `[providers.history]` strategy selects: one row per analyzed commit, the
//! paths it changed against the commit it was compared with, the pure renames
//! among them, and the declarations whose shape changed. It lives in the
//! `rift/` folder of the common git directory, one database file per
//! derivation revision, so two builds sharing a repository each fill and read
//! their own file. When the common git directory refuses that folder, the
//! store sits in the worktree's own state directory instead, and
//! [`HistoryStore::worktree_fallback`] names the refusal.
//!
//! One filler writes a store at a time: the history task holds the store's
//! fill lock exclusively. Every live server holds the store's live lock
//! shared, and a server deletes another revision's files only while it holds
//! that revision's live lock exclusively. Both locks are the std `flock`.
//!
//! The full-text table over commit messages is external content over the
//! commit rows, so every delete removes a row's index entries with the
//! `'delete'` command while the row still holds the message they were indexed
//! from.

mod database;
mod error;
mod lock;
mod record;
mod store;

pub use database::{HeldCommit, StoreFiller, StoreReader, StoreReads, StoredCommit};
pub use error::{StoreError, StoreFault};
pub use record::{ChangedPath, CommitRecord, DeclarationChange, MovedDeclaration, RenamedPath};
pub use store::{HistoryStore, STORE_FOLDER_NAME, StoreLocation, SweptRevisions, WorktreeFallback};

#[cfg(test)]
mod tests;
