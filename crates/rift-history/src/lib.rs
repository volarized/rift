//! Version-control access for Rift: reads a workspace's git repository in
//! place, with no checkout and no `git` subprocess.
//!
//! [`Repository`] opens the repository that versions a workspace root,
//! resolves revision spellings to commits, and serves the committed tree -
//! paths and blob bytes - so a read can be answered from any revision
//! without touching the working tree.

#[cfg(any(test, feature = "fixtures"))]
pub mod fixture;
mod repository;

pub use repository::{
    HistoryError, HistoryFault, REVISION_TREE_ENTRIES_MAX, Repository, ResolvedRevision, TreeFile,
};
