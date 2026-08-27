//! Version-control access for Rift: reads a workspace's git repository in
//! place, with no checkout and no `git` subprocess.
//!
//! [`Repository`] opens the repository that versions a workspace root,
//! resolves revision spellings to commits, serves the committed tree -
//! paths and blob bytes - and lists the first-parent commits that changed
//! one path, so a read can be answered from any revision without touching
//! the working tree.

mod contribution;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixture;
mod repository;

pub use contribution::{
    HistoryContributionAdapter, HistoryContributionError, HistoryContributionViolation,
};
pub use repository::{
    HistoryError, HistoryFault, PathHistory, PathRevision, REVISION_TREE_ENTRIES_MAX, Repository,
    ResolvedRevision, TreeFile,
};
