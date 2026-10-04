//! The commits a history store fill walks, and what each one records and
//! changed.
//!
//! Every read here is a plain object read: no checkout, no `git` subprocess,
//! no filter driver. The walks follow first parents and stop at a shallow
//! clone's boundary the way [`Repository::path_revisions`] does.

use std::path::Path;

use gix::bstr::ByteSlice as _;
use rift_error::{RiftError, errors};

use crate::repository::{
    ChangedPathRecorder, Repository, ResolvedRevision, TreeFile, commit_author, tree_entries,
};

/// One first-parent commit a window holds, with the parent it is compared
/// with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCommit {
    revision: ResolvedRevision,
    parent: Option<ResolvedRevision>,
    boundary: bool,
}

impl WindowCommit {
    /// The commit.
    #[must_use]
    pub const fn revision(&self) -> &ResolvedRevision {
        &self.revision
    }

    /// Its first parent; `None` for a root commit and for a commit on a
    /// shallow clone's boundary.
    #[must_use]
    pub const fn parent(&self) -> Option<&ResolvedRevision> {
        self.parent.as_ref()
    }

    /// Whether the commit sits on a shallow clone's boundary: it has parents
    /// the repository does not hold.
    #[must_use]
    pub const fn is_boundary(&self) -> bool {
        self.boundary
    }
}

/// One tag and the commit it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedCommit {
    name: String,
    revision: ResolvedRevision,
}

impl TaggedCommit {
    /// The tag's short name, such as `v0.0.45`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The commit the tag names, peeled through an annotated tag.
    #[must_use]
    pub const fn revision(&self) -> &ResolvedRevision {
        &self.revision
    }
}

/// The facts one commit records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFacts {
    author_name: String,
    author_email: String,
    committed_at: String,
    committed_seconds: i64,
    message: String,
}

impl CommitFacts {
    /// The author's name, as committed.
    #[must_use]
    pub fn author_name(&self) -> &str {
        &self.author_name
    }

    /// The author's email address, as committed.
    #[must_use]
    pub fn author_email(&self) -> &str {
        &self.author_email
    }

    /// The committer time, an RFC 3339 date-time carrying the recorded offset.
    #[must_use]
    pub fn committed_at(&self) -> &str {
        &self.committed_at
    }

    /// The committer time in seconds since the Unix epoch.
    #[must_use]
    pub const fn committed_seconds(&self) -> i64 {
        self.committed_seconds
    }

    /// The full commit message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One path two trees hold differently, with the blob each side holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedBlob {
    path: String,
    old: Option<TreeFile>,
    new: Option<TreeFile>,
}

impl ChangedBlob {
    /// The workspace-relative path, forward-slash separated.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The blob the compared side holds; `None` for an addition, or where
    /// that side holds a symbolic link or a submodule.
    #[must_use]
    pub const fn old_blob(&self) -> Option<&TreeFile> {
        self.old.as_ref()
    }

    /// The blob the commit holds; `None` for a deletion, including a file the
    /// commit replaced with a symbolic link or a submodule.
    #[must_use]
    pub const fn new_blob(&self) -> Option<&TreeFile> {
        self.new.as_ref()
    }
}

/// The blobs two trees hold differently, and whether the listing stopped at
/// its bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedBlobs {
    blobs: Vec<ChangedBlob>,
    truncated: bool,
}

impl ChangedBlobs {
    /// The changed paths, sorted, each once.
    #[must_use]
    pub fn blobs(&self) -> &[ChangedBlob] {
        &self.blobs
    }

    /// Whether the listing stopped at its path bound, so the trees differ in
    /// paths it does not carry.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }
}

impl Repository {
    /// The common git directory every worktree of this repository shares:
    /// `git rev-parse --git-common-dir`.
    #[must_use]
    pub fn common_directory(&self) -> &Path {
        self.inner.common_dir()
    }

    /// The `HEAD` commit of every live worktree, this workspace's own first,
    /// each once: the main worktree, and each linked worktree whose folder
    /// still exists or which is locked - the set `git worktree list` gives
    /// less its prunable entries.
    ///
    /// A worktree whose `HEAD` resolves to no commit - an unborn branch, or a
    /// worktree whose repository cannot be opened - holds no window and is
    /// left out.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the common git directory's worktree
    /// folder cannot be listed.
    pub fn live_heads(&self) -> Result<Vec<ResolvedRevision>, RiftError> {
        let mut heads: Vec<ResolvedRevision> = Vec::new();
        let mut push = |head: Option<gix::ObjectId>| {
            if let Some(commit) = head
                && heads.iter().all(|held| held.commit != commit)
            {
                heads.push(ResolvedRevision { commit });
            }
        };
        push(head_commit(&self.inner));
        if let Ok(main) = self.inner.main_repo()
            && !main.is_bare()
        {
            push(head_commit(&main));
        }
        let linked = self.inner.worktrees().map_err(|error| {
            errors::history::storage()
                .operation("list worktrees")
                .detail(&error)
                .error()
        })?;
        for proxy in linked {
            let live = proxy.is_locked() || proxy.base().is_ok_and(|base| base.exists());
            if !live {
                continue;
            }
            if let Ok(repository) = proxy.into_repo_with_possibly_inaccessible_worktree() {
                push(head_commit(&repository));
            }
        }
        Ok(heads)
    }

    /// The newest `commits_max` first-parent commits from `start`, newest
    /// first: one commit read per listed commit. A commit on a shallow clone's
    /// boundary has no parent the walk can follow, so it is the last listed.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the object store or the `shallow` file
    /// cannot be read.
    pub fn first_parent_window(
        &self,
        start: &ResolvedRevision,
        commits_max: usize,
    ) -> Result<Vec<WindowCommit>, RiftError> {
        let shallow = self.inner.shallow_commits().map_err(|error| {
            errors::history::storage()
                .operation("read shallow file")
                .detail(&error)
                .error()
        })?;
        let mut window = Vec::new();
        let mut next = Some(start.commit);
        while let Some(id) = next.filter(|_| window.len() < commits_max) {
            let commit = self.inner.find_commit(id).map_err(|error| {
                errors::history::storage()
                    .operation("read commit")
                    .detail(&error)
                    .error()
            })?;
            let boundary = shallow.as_ref().is_some_and(|set| set.contains(&id));
            let parent = if boundary {
                None
            } else {
                commit.parent_ids().next().map(gix::Id::detach)
            };
            window.push(WindowCommit {
                revision: ResolvedRevision { commit: id },
                parent: parent.map(|commit| ResolvedRevision { commit }),
                boundary,
            });
            next = parent;
        }
        Ok(window)
    }

    /// Every tag naming a commit, by short name, sorted: an annotated tag is
    /// peeled through to its commit, and a tag naming a tree or a blob is left
    /// out.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the reference store cannot be read, or
    /// when it holds more than `tags_max` tags.
    pub fn tagged_commits(&self, tags_max: usize) -> Result<Vec<TaggedCommit>, RiftError> {
        let references = self.inner.references().map_err(|error| {
            errors::history::storage()
                .operation("read references")
                .detail(&error)
                .error()
        })?;
        let tags = references.tags().map_err(|error| {
            errors::history::storage()
                .operation("read tags")
                .detail(&error)
                .error()
        })?;
        let mut tagged = Vec::new();
        for (visited, reference) in tags.enumerate() {
            if visited == tags_max {
                return errors::history::too_many_tags()
                    .limit("tags_max")
                    .tags_max(tags_max)
                    .fail();
            }
            let mut reference = reference.map_err(|error| {
                errors::history::storage()
                    .operation("read tag")
                    .detail(&*error)
                    .error()
            })?;
            let name = reference.name().shorten().to_str_lossy().into_owned();
            let Ok(commit) = reference.peel_to_commit() else {
                continue;
            };
            tagged.push(TaggedCommit {
                name,
                revision: ResolvedRevision { commit: commit.id },
            });
        }
        tagged.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(tagged)
    }

    /// The facts `revision`'s commit records: its author, committer time, and
    /// full message.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the commit cannot be read or decoded.
    pub fn commit_facts(&self, revision: &ResolvedRevision) -> Result<CommitFacts, RiftError> {
        let commit = self.inner.find_commit(revision.commit).map_err(|error| {
            errors::history::storage()
                .operation("read commit")
                .detail(&error)
                .error()
        })?;
        let (author_name, author_email) = commit_author(&commit)?;
        let time = commit.time().map_err(|error| {
            errors::history::storage()
                .operation("read commit time")
                .detail(&error)
                .error()
        })?;
        let committed_at = time
            .format(gix::date::time::format::ISO8601_STRICT)
            .map_err(|error| {
                errors::history::storage()
                    .operation("render commit time")
                    .detail(error)
                    .error()
            })?;
        let message = commit.message_raw().map_err(|error| {
            errors::history::storage()
                .operation("read commit message")
                .detail(error)
                .error()
        })?;
        Ok(CommitFacts {
            author_name,
            author_email,
            committed_at,
            committed_seconds: time.seconds,
            message: message.to_str_lossy().into_owned(),
        })
    }

    /// Lists the committed regular files `base` and `head` hold differently,
    /// workspace-relative, passing `includes`, sorted, with the blob each
    /// side holds. A `base` of `None` compares against the empty tree, so
    /// every file `head` holds is an addition.
    ///
    /// The comparison is by blob object id alone and tracks no rename, as
    /// [`Self::changed_files`] compares. The walk stops once `paths_max`
    /// paths pass `includes` and reports itself truncated.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an unreadable object store or a tree
    /// that cannot be decoded.
    pub fn changed_blobs(
        &self,
        base: Option<&ResolvedRevision>,
        head: &ResolvedRevision,
        includes: &dyn Fn(&str) -> bool,
        paths_max: usize,
    ) -> Result<ChangedBlobs, RiftError> {
        let head_tree = self.commit_tree(head)?;
        let base_tree = base.map(|base| self.commit_tree(base)).transpose()?;
        let mut recorder = ChangedPathRecorder::new(self.prefix.as_bytes(), includes, paths_max);
        let base_entries = match &base_tree {
            Some(tree) => tree_entries(tree),
            None => gix::objs::TreeRefIter::from_bytes(&[], head_tree.id.kind()),
        };
        let outcome = gix::diff::tree(
            base_entries,
            tree_entries(&head_tree),
            &mut gix::diff::tree::State::default(),
            &self.inner,
            &mut recorder,
        );
        match outcome {
            Ok(()) => {}
            Err(gix::diff::tree::Error::Cancelled) if recorder.truncated => {}
            Err(error) => {
                return errors::history::storage()
                    .operation("compare commit trees")
                    .detail(&error)
                    .fail();
            }
        }
        let truncated = recorder.truncated;
        let blobs = recorder
            .into_changes()
            .into_iter()
            .map(|change| ChangedBlob {
                old: change.old.map(|blob| TreeFile {
                    path: change.path.clone(),
                    blob,
                }),
                new: change.new.map(|blob| TreeFile {
                    path: change.path.clone(),
                    blob,
                }),
                path: change.path,
            })
            .collect();
        Ok(ChangedBlobs { blobs, truncated })
    }
}

/// The commit `repository`'s `HEAD` names; `None` for an unborn branch or a
/// `HEAD` that resolves to nothing.
fn head_commit(repository: &gix::Repository) -> Option<gix::ObjectId> {
    repository.head_id().ok().map(gix::Id::detach)
}

#[cfg(test)]
mod tests;
