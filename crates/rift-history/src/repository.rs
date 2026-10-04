//! One workspace's git repository, opened in place for revision-addressed reads.

use std::path::{Path, PathBuf};

use rift_error::{RiftError, ctx, errors};

/// Tree entries one revision listing may visit, files and directories alike.
/// The traversal refuses a larger tree rather than truncating it silently.
pub const REVISION_TREE_ENTRIES_MAX: usize = 65_536;

/// One revision resolved to the commit it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRevision {
    pub(crate) commit: gix::ObjectId,
}

impl ResolvedRevision {
    /// The full lowercase hex commit id - the durable spelling the
    /// repository records for this revision.
    #[must_use]
    pub fn commit_id(&self) -> String {
        self.commit.to_string()
    }
}

/// One committed regular file inside the workspace: its workspace-relative
/// path and the blob that holds its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFile {
    pub(crate) path: String,
    pub(crate) blob: gix::ObjectId,
}

impl TreeFile {
    /// The workspace-relative path, forward-slash separated.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The blob's full lowercase hex object id - the durable spelling for
    /// the exact committed bytes, usable as a content cache key.
    #[must_use]
    pub fn blob_id(&self) -> String {
        self.blob.to_string()
    }
}

/// One first-parent commit that changed a path's tree entry: the commit's
/// recorded facts and the blob its tree holds at the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRevision {
    commit_id: String,
    timestamp: String,
    summary: Option<String>,
    author_name: String,
    author_email: String,
    blob: Option<TreeFile>,
}

impl PathRevision {
    /// The full lowercase hex commit id.
    #[must_use]
    pub fn commit_id(&self) -> &str {
        &self.commit_id
    }

    /// The commit's committer time, an RFC 3339 date-time carrying the
    /// recorded fixed offset.
    #[must_use]
    pub fn timestamp(&self) -> &str {
        &self.timestamp
    }

    /// The commit message's summary line; `None` for an empty message.
    #[must_use]
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

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

    /// The blob the commit's tree holds at the path; `None` when the commit
    /// removed the path.
    #[must_use]
    pub const fn blob(&self) -> Option<&TreeFile> {
        self.blob.as_ref()
    }
}

/// The first-parent commits that changed one path's tree entry, newest
/// first, and whether the walk covered the path's whole history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathHistory {
    revisions: Vec<PathRevision>,
    complete: bool,
}

impl PathHistory {
    /// The commits that changed the path's entry, newest first.
    #[must_use]
    pub fn revisions(&self) -> &[PathRevision] {
        &self.revisions
    }

    /// Whether the walk examined the path's whole first-parent history.
    /// `false` when the examination bound or a shallow clone's boundary
    /// stopped the walk first, so touching commits older than the listed
    /// ones may exist.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }
}

/// The committed files two revisions hold differently, and whether the
/// comparison listed all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFiles {
    pub(crate) paths: Vec<String>,
    pub(crate) truncated: bool,
}

impl ChangedFiles {
    /// The workspace-relative paths the two revisions hold differently,
    /// sorted and deduplicated.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// Whether the comparison stopped at its path bound. `true` means the
    /// two revisions differ in paths this listing does not carry.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }
}

/// The git repository that versions one workspace root.
///
/// The workspace may sit below the repository's working-tree root; listings
/// then serve only entries inside the workspace, re-relativized to it.
#[derive(Debug)]
pub struct Repository {
    pub(crate) inner: gix::Repository,
    root: PathBuf,
    /// The workspace root's forward-slash path inside the repository's
    /// working tree; empty when the workspace is that root.
    pub(crate) prefix: String,
}

impl Repository {
    /// Opens the repository that versions `root`, discovering it upward the
    /// way git itself does.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when no repository versions `root`, or when
    /// the filesystem cannot be read while discovering one.
    pub fn open(root: &Path) -> Result<Self, RiftError> {
        let root = std::fs::canonicalize(root).map_err(|error| {
            errors::history::storage()
                .operation("canonicalize workspace root")
                .detail(&error)
                .error()
                .with(ctx::workspace(root))
        })?;
        let unversioned = || {
            errors::history::unversioned()
                .workspace(&root)
                .requires(
                    "a git repository - run `git init`, or omit `rev` to read the current tree",
                )
                .error()
        };
        let inner = gix::discover(&root).map_err(|_| unversioned())?;
        let workdir = inner.workdir().ok_or_else(unversioned)?;
        let workdir = std::fs::canonicalize(workdir).map_err(|error| {
            errors::history::storage()
                .operation("canonicalize repository root")
                .detail(&error)
                .error()
        })?;
        let prefix = root
            .strip_prefix(&workdir)
            .map_err(|_| unversioned())?
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        Ok(Self {
            inner,
            root,
            prefix,
        })
    }

    /// The canonical workspace root this repository was opened for.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The existing canonical root of this repository's main worktree.
    ///
    /// Returns `None` when the repository is bare or its main worktree is
    /// absent, unreadable, or no longer a directory.
    #[must_use]
    pub fn main_worktree_root(&self) -> Option<PathBuf> {
        main_worktree_root(&self.inner)
    }

    /// The Git index lock for this workspace's worktree.
    ///
    /// Linked worktrees have separate index locks in their own Git directories.
    #[must_use]
    pub fn index_lock_path(&self) -> PathBuf {
        self.inner.index_path().with_extension("lock")
    }

    /// Whether `root` itself is a linked Git worktree, excluding submodules.
    ///
    /// Classification examines the root's `.git` entry without discovering a parent.
    #[must_use]
    pub fn is_linked_worktree(root: &Path) -> bool {
        matches!(
            gix::discover::is_git(&root.join(gix::discover::DOT_GIT_DIR)),
            Ok(gix::discover::repository::Kind::WorkTree {
                linked_git_dir: Some(_),
            })
        )
    }

    /// Resolves one revision spelling - a branch, tag, or commit id - to
    /// the commit it names.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the spelling resolves to nothing, or to
    /// an object that is not a commit.
    pub fn resolve(&self, rev: &str) -> Result<ResolvedRevision, RiftError> {
        let unknown = || {
            errors::history::revision_unknown()
                .rev(rev)
                .requires("a revision the repository resolves - a branch, tag, or commit id")
                .error()
        };
        let id = self
            .inner
            .rev_parse_single(rev.as_bytes().as_bstr())
            .map_err(|_| unknown())?;
        let object = id.object().map_err(|_| unknown())?;
        let commit = object
            .peel_to_kind(gix::object::Kind::Commit)
            .map_err(|_| {
                errors::history::revision_not_commit()
                    .rev(rev)
                    .resolved_kind(object_kind(&self.inner, id.detach()))
                    .requires("a revision that names a commit")
                    .error()
            })?;
        Ok(ResolvedRevision { commit: commit.id })
    }

    /// Lists the revision's committed regular files inside the workspace
    /// whose workspace-relative path passes `includes`, sorted by path.
    ///
    /// Symbolic links and submodules are never listed. The traversal counts
    /// every visited tree entry against `entries_max` - callers pass
    /// [`REVISION_TREE_ENTRIES_MAX`] - stops descending once the budget is
    /// spent, and refuses the listing rather than truncating it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an unreadable object store, a committed
    /// non-UTF-8 path inside the workspace, or a tree over the entry bound.
    pub fn tree_files(
        &self,
        revision: &ResolvedRevision,
        includes: &dyn Fn(&str) -> bool,
        entries_max: usize,
    ) -> Result<Vec<TreeFile>, RiftError> {
        let commit = self.inner.find_commit(revision.commit).map_err(|error| {
            errors::history::storage()
                .operation("read commit")
                .detail(&error)
                .error()
        })?;
        let tree = commit.tree().map_err(|error| {
            errors::history::storage()
                .operation("read commit tree")
                .detail(&error)
                .error()
        })?;
        let mut recorder = BoundedRecorder::new(entries_max);
        tree.traverse().depthfirst(&mut recorder).map_err(|error| {
            errors::history::storage()
                .operation("walk commit tree")
                .detail(&error)
                .error()
        })?;
        if recorder.exhausted {
            return errors::history::tree_too_large()
                .limit("revision_tree_entries_max")
                .entries_max(entries_max)
                .fail();
        }
        let mut files = Vec::new();
        for record in recorder.inner.records {
            if !record.mode.is_blob() {
                continue;
            }
            let Some(path) = self.workspace_relative(record.filepath.as_ref())? else {
                continue;
            };
            if !includes(&path) {
                continue;
            }
            files.push(TreeFile {
                path,
                blob: record.oid,
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    /// Reads one committed file's bytes, refusing a blob over `bytes_max`
    /// before loading it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an oversized blob or an unreadable
    /// object store.
    pub fn blob_bytes(&self, file: &TreeFile, bytes_max: usize) -> Result<Vec<u8>, RiftError> {
        let header = self.inner.find_header(file.blob).map_err(|error| {
            errors::history::storage()
                .operation("read blob header")
                .detail(&error)
                .error()
        })?;
        if header.size() > bytes_max as u64 {
            return errors::history::blob_too_large()
                .path(Path::new(&file.path))
                .bytes_max(bytes_max)
                .size(header.size())
                .fail();
        }
        let object = self.inner.find_object(file.blob).map_err(|error| {
            errors::history::storage()
                .operation("read blob")
                .detail(&error)
                .error()
        })?;
        Ok(object.detach().data)
    }

    /// Lists the committed regular files `base` and `head` hold
    /// differently, workspace-relative, passing `includes`, sorted and
    /// deduplicated.
    ///
    /// The comparison is by blob object id alone, the same policy
    /// `path_revisions` applies. gix's tree comparison "does not do rename
    /// tracking" (`gix-diff-0.66.0/src/tree/function.rs:23`), so a file that
    /// moved is one deletion beside one addition; pairing the two back up is
    /// the caller's decision, taken over the declarations inside them.
    /// A path that is a symbolic link or a submodule on both sides is never
    /// listed; one that held a file on either side is, so a file replaced by
    /// a link is listed as the file's deletion. A committed path whose bytes
    /// are not UTF-8 names no readable file, so the comparison passes over it.
    ///
    /// The walk stops once `paths_max` paths pass `includes` and reports
    /// itself truncated, so one comparison's work stays proportional to that
    /// bound plus the tree breadth already queued when it was reached.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an unreadable object store or a tree
    /// that cannot be decoded.
    pub fn changed_files(
        &self,
        base: &ResolvedRevision,
        head: &ResolvedRevision,
        includes: &dyn Fn(&str) -> bool,
        paths_max: usize,
    ) -> Result<ChangedFiles, RiftError> {
        let base_tree = self.commit_tree(base)?;
        let head_tree = self.commit_tree(head)?;
        let mut recorder = ChangedPathRecorder::new(self.prefix.as_bytes(), includes, paths_max);
        let outcome = gix::diff::tree(
            tree_entries(&base_tree),
            tree_entries(&head_tree),
            &mut gix::diff::tree::State::default(),
            &self.inner,
            &mut recorder,
        );
        // The delegate stops the walk by breaking, which gix reports as a
        // cancelled comparison; the recorder's own flag says whether the
        // break was this bound rather than a real failure.
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
        let paths = recorder
            .into_changes()
            .into_iter()
            .map(|change| change.path)
            .collect();
        Ok(ChangedFiles { paths, truncated })
    }

    /// The tree one resolved revision's commit points at.
    pub(crate) fn commit_tree(
        &self,
        revision: &ResolvedRevision,
    ) -> Result<gix::Tree<'_>, RiftError> {
        self.inner
            .find_commit(revision.commit)
            .map_err(|error| {
                errors::history::storage()
                    .operation("read commit")
                    .detail(&error)
                    .error()
            })?
            .tree()
            .map_err(|error| {
                errors::history::storage()
                    .operation("read commit tree")
                    .detail(&error)
                    .error()
            })
    }

    /// Lists the first-parent commits from `revision` whose tree changed
    /// `path`'s entry, newest first, examining at most `revisions_max`
    /// commits - one commit read and one tree-entry read per examined
    /// commit.
    ///
    /// The comparison is by blob object id alone: no diffing, no content
    /// read. A non-blob entry at the path - a directory, a symbolic link,
    /// a submodule - counts as absent, the same policy `tree_files`
    /// applies. A walk whose examination bound is spent before the first
    /// commit reports itself incomplete and lists the newest touching
    /// commits only.
    ///
    /// A shallow clone holds no commit past the boundary its `shallow`
    /// file records, so a commit listed there is walked as having no
    /// parent and the walk reports itself incomplete at it. The set is read
    /// once per walk: gix reads the whole file, one commit id per line, so
    /// its size bounds the set, and each examined commit costs one scan of
    /// it. A parent absent for any other reason fails the read.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the object store or the `shallow`
    /// file cannot be read.
    pub fn path_revisions(
        &self,
        revision: &ResolvedRevision,
        path: &str,
        revisions_max: usize,
    ) -> Result<PathHistory, RiftError> {
        let repository_path = self.repository_path(path);
        let shallow = self.inner.shallow_commits().map_err(|error| {
            errors::history::storage()
                .operation("read shallow file")
                .detail(&error)
                .error()
        })?;
        let mut commit = self.inner.find_commit(revision.commit).map_err(|error| {
            errors::history::storage()
                .operation("read commit")
                .detail(&error)
                .error()
        })?;
        let mut entry = blob_entry(&commit, &repository_path)?;
        let mut revisions = Vec::new();
        let mut complete = false;
        for _ in 0..revisions_max {
            let at_boundary = at_shallow_boundary(shallow.as_ref(), &commit);
            let parent_id = if at_boundary {
                None
            } else {
                commit.parent_ids().next().map(gix::Id::detach)
            };
            let (parent, parent_entry) = match parent_id {
                Some(id) => {
                    let parent = self.inner.find_commit(id).map_err(|error| {
                        errors::history::storage()
                            .operation("read commit")
                            .detail(&error)
                            .error()
                    })?;
                    let parent_entry = blob_entry(&parent, &repository_path)?;
                    (Some(parent), parent_entry)
                }
                None => (None, None),
            };
            if entry != parent_entry {
                revisions.push(path_revision(&commit, path, entry)?);
            }
            let Some(parent) = parent else {
                complete = !at_boundary;
                break;
            };
            commit = parent;
            entry = parent_entry;
        }
        Ok(PathHistory {
            revisions,
            complete,
        })
    }

    /// The repository-relative spelling of one workspace-relative path.
    fn repository_path(&self, path: &str) -> String {
        if self.prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{}/{path}", self.prefix)
        }
    }

    /// The workspace-relative form of one repository-relative path: `None`
    /// when the path lies outside the workspace, an error when its bytes
    /// inside the workspace are not UTF-8.
    fn workspace_relative(&self, filepath: &[u8]) -> Result<Option<String>, RiftError> {
        let Some(relative) = strip_workspace_prefix(filepath, self.prefix.as_bytes()) else {
            return Ok(None);
        };
        utf8_path(relative, filepath).map(Some)
    }
}

/// The bytes of `filepath` below the workspace prefix, or `None` for a path
/// outside the workspace. An empty prefix - the workspace at the repository
/// root - keeps every path.
pub(crate) fn strip_workspace_prefix<'a>(filepath: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if prefix.is_empty() {
        return Some(filepath);
    }
    filepath.strip_prefix(prefix)?.strip_prefix(b"/")
}

/// The blob object id one commit's tree holds at `repository_path`; `None`
/// for an absent path or a non-blob entry.
fn blob_entry(
    commit: &gix::Commit<'_>,
    repository_path: &str,
) -> Result<Option<gix::ObjectId>, RiftError> {
    let tree = commit.tree().map_err(|error| {
        errors::history::storage()
            .operation("read commit tree")
            .detail(&error)
            .error()
    })?;
    let entry = tree
        .lookup_entry_by_path(repository_path)
        .map_err(|error| {
            errors::history::storage()
                .operation("read tree entry")
                .detail(&error)
                .error()
        })?;
    Ok(entry
        .filter(|entry| entry.mode().is_blob())
        .map(|entry| entry.object_id()))
}

/// Whether `commit` sits on the clone's shallow boundary: listed in the
/// `shallow` set, so the repository holds none of its parents. No set - the
/// clone is not shallow - puts no commit on a boundary.
fn at_shallow_boundary(shallow: Option<&gix::shallow::Commits>, commit: &gix::Commit<'_>) -> bool {
    shallow.is_some_and(|set| set.contains(&commit.id))
}

/// One touching commit's recorded facts, with the blob its tree holds at
/// the path.
fn path_revision(
    commit: &gix::Commit<'_>,
    path: &str,
    blob: Option<gix::ObjectId>,
) -> Result<PathRevision, RiftError> {
    let time = commit.time().map_err(|error| {
        errors::history::storage()
            .operation("read commit time")
            .detail(&error)
            .error()
    })?;
    let timestamp = time
        .format(gix::date::time::format::ISO8601_STRICT)
        .map_err(|error| {
            errors::history::storage()
                .operation("render commit time")
                .detail(&error)
                .error()
        })?;
    let summary = commit
        .message()
        .map_err(|error| {
            errors::history::storage()
                .operation("read commit message")
                .detail(&error)
                .error()
        })?
        .summary()
        .to_string();
    let (author_name, author_email) = commit_author(commit)?;
    Ok(PathRevision {
        commit_id: commit.id.to_string(),
        timestamp,
        summary: (!summary.is_empty()).then_some(summary),
        author_name,
        author_email,
        blob: blob.map(|blob| TreeFile {
            path: path.to_owned(),
            blob,
        }),
    })
}

/// The name and email address `commit`'s author line records, trimmed of the
/// whitespace the signature parser keeps.
pub(crate) fn commit_author(commit: &gix::Commit<'_>) -> Result<(String, String), RiftError> {
    let author = commit.author().map_err(|error| {
        errors::history::storage()
            .operation("read commit author")
            .detail(&error)
            .error()
    })?;
    Ok((
        author.name.to_str_lossy().trim().to_owned(),
        author.email.to_str_lossy().trim().to_owned(),
    ))
}

/// One committed path's UTF-8 form. The refusal renders the full
/// repository-relative spelling, so the reader sees the path git records.
fn utf8_path(relative: &[u8], filepath: &[u8]) -> Result<String, RiftError> {
    std::str::from_utf8(relative)
        .map(str::to_owned)
        .map_err(|_| {
            errors::history::path_unrepresentable()
                .path(String::from_utf8_lossy(filepath).into_owned())
                .error()
        })
}

/// One tree's entries, as the comparison reads them.
pub(crate) fn tree_entries<'tree>(tree: &'tree gix::Tree<'_>) -> gix::objs::TreeRefIter<'tree> {
    gix::objs::TreeRefIter::from_bytes(&tree.data, tree.id.kind())
}

/// One blob path two trees hold differently, with the blob id each side
/// holds there.
#[derive(Debug)]
pub(crate) struct RecordedChange {
    pub(crate) path: String,
    pub(crate) old: Option<gix::ObjectId>,
    pub(crate) new: Option<gix::ObjectId>,
}

impl RecordedChange {
    /// The blob id each side of one visited tree difference holds: an entry
    /// that is no blob on a side - a tree, a symbolic link, a submodule -
    /// holds none there.
    ///
    /// Each side is read from its own mode. gix reports a file turned into a
    /// link as one modification, "turning a file into a symbolic link adjusts
    /// its mode" (`gix-diff-0.66.0/src/tree/visit.rs:43`), and
    /// `Change::entry_mode` answers the new side's mode alone.
    fn blob_sides(
        change: &gix::diff::tree::visit::Change,
    ) -> (Option<gix::ObjectId>, Option<gix::ObjectId>) {
        use gix::diff::tree::visit::Change;
        let blob =
            |mode: gix::objs::tree::EntryMode, id: gix::ObjectId| mode.is_blob().then_some(id);
        match change {
            Change::Addition {
                entry_mode, oid, ..
            } => (None, blob(*entry_mode, *oid)),
            Change::Deletion {
                entry_mode, oid, ..
            } => (blob(*entry_mode, *oid), None),
            Change::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
            } => (
                blob(*previous_entry_mode, *previous_oid),
                blob(*entry_mode, *oid),
            ),
        }
    }
}

/// A [`gix::diff::tree::Recorder`] behind a path budget: it tracks the
/// compared path the way gix's own recorder does, and keeps the blob changes
/// inside the workspace that pass `includes`, up to `paths_max`. The budget
/// counts accepted paths alone, so a revision pair differing only in files
/// the workspace policy excludes never reports itself truncated.
pub(crate) struct ChangedPathRecorder<'a> {
    inner: gix::diff::tree::Recorder,
    prefix: &'a [u8],
    includes: &'a dyn Fn(&str) -> bool,
    paths_max: usize,
    changes: Vec<RecordedChange>,
    pub(crate) truncated: bool,
}

impl<'a> ChangedPathRecorder<'a> {
    pub(crate) fn new(
        prefix: &'a [u8],
        includes: &'a dyn Fn(&str) -> bool,
        paths_max: usize,
    ) -> Self {
        Self {
            inner: gix::diff::tree::Recorder::default(),
            prefix,
            includes,
            paths_max,
            changes: Vec::new(),
            truncated: false,
        }
    }

    /// The recorded changes, sorted by path, one per path. The comparison
    /// orders a folder's name as if it ended in `/`, so a blob that replaced a
    /// folder is one path and the folder's files are others; only a tree that
    /// names one path twice, which `git fsck` reports as `duplicateEntries`,
    /// records it twice, and its change keeps the first blob id each side
    /// names.
    pub(crate) fn into_changes(self) -> Vec<RecordedChange> {
        let mut changes = self.changes;
        changes.sort_by(|left, right| left.path.cmp(&right.path));
        let mut merged: Vec<RecordedChange> = Vec::with_capacity(changes.len());
        for change in changes {
            match merged.last_mut() {
                Some(last) if last.path == change.path => {
                    last.old = last.old.or(change.old);
                    last.new = last.new.or(change.new);
                }
                _ => merged.push(change),
            }
        }
        merged
    }

    /// The workspace-relative path this change names, or `None` for a path
    /// outside the workspace, one whose bytes are not UTF-8, or one the
    /// caller's predicate drops.
    fn accepted(&self, filepath: &[u8]) -> Option<String> {
        let relative = strip_workspace_prefix(filepath, self.prefix)?;
        let relative = std::str::from_utf8(relative).ok()?;
        (self.includes)(relative).then(|| relative.to_owned())
    }
}

impl gix::diff::tree::Visit for ChangedPathRecorder<'_> {
    fn pop_front_tracked_path_and_set_current(&mut self) {
        self.inner.pop_front_tracked_path_and_set_current();
    }

    fn push_back_tracked_path_component(&mut self, component: &gix::bstr::BStr) {
        self.inner.push_back_tracked_path_component(component);
    }

    fn push_path_component(&mut self, component: &gix::bstr::BStr) {
        self.inner.push_path_component(component);
    }

    fn pop_path_component(&mut self) {
        self.inner.pop_path_component();
    }

    fn visit(&mut self, change: gix::diff::tree::visit::Change) -> ChangeVisitAction {
        let (old, new) = RecordedChange::blob_sides(&change);
        if old.is_none() && new.is_none() {
            // Neither side holds a file: a tree added or deleted whole is
            // announced before its blobs, which the walk goes on to announce
            // one by one, and a link or submodule holds no bytes to compare.
            return std::ops::ControlFlow::Continue(());
        }
        let Some(path) = self.accepted(self.inner.path()) else {
            return std::ops::ControlFlow::Continue(());
        };
        if self.changes.len() >= self.paths_max {
            self.truncated = true;
            return std::ops::ControlFlow::Break(());
        }
        self.changes.push(RecordedChange { path, old, new });
        std::ops::ControlFlow::Continue(())
    }
}

/// The comparison's per-change instruction, spelled once.
type ChangeVisitAction = std::ops::ControlFlow<()>;

/// A [`gix::traverse::tree::Recorder`] behind an entry budget: every visited
/// entry spends one unit, and a spent budget stops descent and recording, so
/// the walk's work stays proportional to `entries_max` plus the breadth
/// already queued when the budget ran out.
struct BoundedRecorder {
    inner: gix::traverse::tree::Recorder,
    budget: usize,
    exhausted: bool,
}

impl BoundedRecorder {
    fn new(entries_max: usize) -> Self {
        Self {
            inner: gix::traverse::tree::Recorder::default(),
            budget: entries_max,
            exhausted: false,
        }
    }

    /// Spends one unit of the entry budget; `false` once it is exhausted.
    fn spend(&mut self) -> bool {
        if self.budget == 0 {
            self.exhausted = true;
            return false;
        }
        self.budget -= 1;
        true
    }
}

impl gix::traverse::tree::Visit for BoundedRecorder {
    fn pop_back_tracked_path_and_set_current(&mut self) {
        self.inner.pop_back_tracked_path_and_set_current();
    }

    fn pop_front_tracked_path_and_set_current(&mut self) {
        self.inner.pop_front_tracked_path_and_set_current();
    }

    fn push_back_tracked_path_component(&mut self, component: &gix::bstr::BStr) {
        self.inner.push_back_tracked_path_component(component);
    }

    fn push_path_component(&mut self, component: &gix::bstr::BStr) {
        self.inner.push_path_component(component);
    }

    fn pop_path_component(&mut self) {
        self.inner.pop_path_component();
    }

    fn visit_tree(&mut self, entry: &gix::objs::tree::EntryRef<'_>) -> TreeVisitAction {
        if !self.spend() {
            // The budget is spent: never descend into further subtrees.
            return std::ops::ControlFlow::Break(());
        }
        self.inner.visit_tree(entry)
    }

    fn visit_nontree(&mut self, entry: &gix::objs::tree::EntryRef<'_>) -> TreeVisitAction {
        if !self.spend() {
            return std::ops::ControlFlow::Continue(true);
        }
        self.inner.visit_nontree(entry)
    }
}

/// The traversal's per-entry instruction, spelled once.
type TreeVisitAction = std::ops::ControlFlow<(), bool>;

/// The kind of the object a revision resolved to, for the refusal that
/// names it.
fn object_kind(repository: &gix::Repository, id: gix::ObjectId) -> String {
    repository
        .find_header(id)
        .map_or_else(|_| "unknown".to_owned(), |header| header.kind().to_string())
}

/// The existing canonical root of `repository`'s main worktree, when it is
/// a readable directory.
fn main_worktree_root(repository: &gix::Repository) -> Option<PathBuf> {
    let main = repository.main_repo().ok()?;
    if main.is_bare() || !main.worktree()?.dot_git_exists() {
        return None;
    }
    let root = std::fs::canonicalize(main.workdir()?).ok()?;
    std::fs::read_dir(&root).ok()?;
    root.is_dir().then_some(root)
}

use gix::bstr::ByteSlice as _;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{commit_all, git, init};
    use std::fs;

    fn repository_fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temp dir");
        init(directory.path());
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n").expect("source");
        commit_all(directory.path(), "introduce beacon");
        directory
    }

    fn add_linked_worktree(repository: &Path, parent: &Path) -> PathBuf {
        let linked = parent.join("linked");
        git(
            repository,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().expect("temporary path is UTF-8"),
                "HEAD",
            ],
        );
        linked
    }

    #[test]
    fn linked_worktree_classification_preserves_independent_repositories_and_submodules() {
        let main = repository_fixture();
        let linked_parent = tempfile::tempdir().expect("linked parent");
        let linked = add_linked_worktree(main.path(), linked_parent.path());
        assert!(Repository::is_linked_worktree(&linked));
        assert!(!Repository::is_linked_worktree(main.path()));
        let ordinary = tempfile::tempdir().expect("ordinary folder");
        assert!(!Repository::is_linked_worktree(ordinary.path()));
        let independent = repository_fixture();
        git(
            main.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                independent
                    .path()
                    .to_str()
                    .expect("temporary path is UTF-8"),
                "module",
            ],
        );
        assert!(!Repository::is_linked_worktree(&main.path().join("module")));
    }

    #[test]
    fn main_worktree_root_is_same_from_main_and_linked_worktrees() {
        let main = repository_fixture();
        let linked_parent = tempfile::tempdir().expect("linked parent");
        let linked = add_linked_worktree(main.path(), linked_parent.path());
        let canonical_main = fs::canonicalize(main.path()).expect("main root is readable");

        let main_repository = Repository::open(main.path()).expect("main repository");
        let linked_repository = Repository::open(&linked).expect("linked repository");
        let main_common = fs::canonicalize(main_repository.common_directory())
            .expect("main common directory is readable");
        let linked_common = fs::canonicalize(linked_repository.common_directory())
            .expect("linked common directory is readable");
        assert_eq!(
            main_repository.main_worktree_root(),
            Some(canonical_main.clone())
        );
        assert_eq!(linked_repository.main_worktree_root(), Some(canonical_main));
        assert_eq!(main_common, linked_common);
        assert_eq!(
            main_repository.index_lock_path(),
            main_repository.common_directory().join("index.lock")
        );
        assert_ne!(
            linked_repository.index_lock_path(),
            main_repository.index_lock_path()
        );
        assert_eq!(
            linked_repository.index_lock_path(),
            linked_repository.inner.git_dir().join("index.lock")
        );
    }

    #[test]
    fn main_worktree_root_is_none_for_bare_repository() {
        let bare = tempfile::tempdir().expect("bare repository");
        git(bare.path(), &["init", "--bare", "-q"]);
        let repository = gix::discover(bare.path()).expect("bare repository opens");

        assert!(repository.is_bare());
        assert_eq!(main_worktree_root(&repository), None);
    }

    #[test]
    fn main_worktree_root_is_none_after_main_worktree_is_removed() {
        let main = repository_fixture();
        let main_root = main.path().to_path_buf();
        let linked_parent = tempfile::tempdir().expect("linked parent");
        let linked = add_linked_worktree(&main_root, linked_parent.path());
        let repository = Repository::open(&linked).expect("linked repository");

        fs::remove_dir_all(&main_root).expect("remove main worktree");

        assert_eq!(repository.main_worktree_root(), None);
    }

    #[test]
    fn repository_open_refuses_an_ordinary_folder() {
        let directory = tempfile::tempdir().expect("ordinary folder");

        assert!(Repository::open(directory.path()).is_err());
    }

    fn include_all(_: &str) -> bool {
        true
    }

    /// The paths two revisions differ in: the added file, the modified one,
    /// and the removed one, with the untouched file left out.
    #[test]
    fn test_changed_files_lists_additions_modifications_and_deletions() {
        let directory = repository_fixture();
        let root = directory.path();
        fs::write(root.join("steady.rs"), "pub fn steady() {}\n").expect("source");
        fs::write(root.join("gone.rs"), "pub fn gone() {}\n").expect("source");
        commit_all(root, "baseline");
        git(root, &["tag", "baseline"]);
        fs::write(root.join("lib.rs"), "pub fn beacon(flag: bool) {}\n").expect("source");
        fs::create_dir_all(root.join("nested")).expect("directory");
        fs::write(root.join("nested/added.rs"), "pub fn added() {}\n").expect("source");
        fs::remove_file(root.join("gone.rs")).expect("removal");
        commit_all(root, "change the tree");

        let repository = Repository::open(root).expect("repository");
        let base = repository.resolve("baseline").expect("base resolves");
        let head = repository.resolve("HEAD").expect("head resolves");
        let changed = repository
            .changed_files(&base, &head, &include_all, 64)
            .expect("comparison");

        assert_eq!(changed.paths(), ["gone.rs", "lib.rs", "nested/added.rs"]);
        assert!(!changed.is_truncated());
    }

    /// Two revisions holding one tree answer no changed path at all.
    #[test]
    fn test_changed_files_lists_nothing_for_two_equal_trees() {
        let directory = repository_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let changed = repository
            .changed_files(&head, &head, &include_all, 64)
            .expect("comparison");
        assert!(changed.paths().is_empty());
        assert!(!changed.is_truncated());
    }

    /// The path bound counts the paths that pass `includes`, and a
    /// comparison that reaches it answers what fit and says so.
    #[test]
    fn test_changed_files_stops_at_the_path_bound_and_reports_truncation() {
        let directory = repository_fixture();
        let root = directory.path();
        git(root, &["tag", "baseline"]);
        for index in 0..4 {
            fs::write(root.join(format!("added{index}.rs")), "pub fn added() {}\n")
                .expect("source");
        }
        commit_all(root, "add four files");

        let repository = Repository::open(root).expect("repository");
        let base = repository.resolve("baseline").expect("base resolves");
        let head = repository.resolve("HEAD").expect("head resolves");
        let changed = repository
            .changed_files(&base, &head, &include_all, 2)
            .expect("comparison");

        assert_eq!(changed.paths().len(), 2);
        assert!(changed.is_truncated());
    }

    /// The predicate drops paths before the budget is spent, so a pair
    /// differing only in excluded files answers empty and untruncated.
    #[test]
    fn test_changed_files_applies_includes_before_the_path_bound() {
        let directory = repository_fixture();
        let root = directory.path();
        git(root, &["tag", "baseline"]);
        fs::create_dir_all(root.join("vendor")).expect("directory");
        for index in 0..4 {
            fs::write(
                root.join(format!("vendor/dep{index}.rs")),
                "pub fn vendored() {}\n",
            )
            .expect("source");
        }
        commit_all(root, "add vendored files");

        let repository = Repository::open(root).expect("repository");
        let base = repository.resolve("baseline").expect("base resolves");
        let head = repository.resolve("HEAD").expect("head resolves");
        let outside_vendor = |path: &str| !path.starts_with("vendor/");
        let changed = repository
            .changed_files(&base, &head, &outside_vendor, 2)
            .expect("comparison");

        assert!(changed.paths().is_empty());
        assert!(!changed.is_truncated());
    }

    /// A workspace below the repository root lists changed paths relative to
    /// itself, and never a changed path outside it.
    #[test]
    fn test_changed_files_relativizes_to_the_workspace_below_the_repository_root() {
        let directory = repository_fixture();
        let root = directory.path();
        fs::create_dir_all(root.join("inner")).expect("directory");
        fs::write(root.join("inner/lib.rs"), "pub fn inner() {}\n").expect("source");
        commit_all(root, "baseline");
        git(root, &["tag", "baseline"]);
        fs::write(root.join("inner/lib.rs"), "pub fn inner(flag: bool) {}\n").expect("source");
        fs::write(root.join("lib.rs"), "pub fn beacon(flag: bool) {}\n").expect("source");
        commit_all(root, "change both");

        let repository = Repository::open(&root.join("inner")).expect("repository");
        let base = repository.resolve("baseline").expect("base resolves");
        let head = repository.resolve("HEAD").expect("head resolves");
        let changed = repository
            .changed_files(&base, &head, &include_all, 64)
            .expect("comparison");

        assert_eq!(changed.paths(), ["lib.rs"]);
    }

    /// A symbolic link is never a comparison's changed path, the same policy
    /// the tree listing applies.
    #[cfg(unix)]
    #[test]
    fn test_changed_files_never_lists_a_symbolic_link() {
        let directory = repository_fixture();
        let root = directory.path();
        git(root, &["tag", "baseline"]);
        std::os::unix::fs::symlink("lib.rs", root.join("link.rs")).expect("symlink");
        commit_all(root, "add a link");

        let repository = Repository::open(root).expect("repository");
        let base = repository.resolve("baseline").expect("base resolves");
        let head = repository.resolve("HEAD").expect("head resolves");
        let changed = repository
            .changed_files(&base, &head, &include_all, 64)
            .expect("comparison");

        assert!(changed.paths().is_empty());
    }

    /// A commit whose tree names a subtree the object store does not hold fails
    /// the comparison rather than answering a partial listing.
    #[test]
    fn test_changed_files_refuses_a_tree_the_object_store_cannot_read() {
        let directory = repository_fixture();
        let root = directory.path();
        crate::fixture::commit_missing_subtree(root, "refs/heads/broken");

        let repository = Repository::open(root).expect("repository");
        let base = repository.resolve("main").expect("base resolves");
        let broken = repository.resolve("broken").expect("broken resolves");
        let error = repository
            .changed_files(&base, &broken, &include_all, 64)
            .expect_err("an unreadable tree must refuse");

        assert_eq!(error.slug(), errors::history::storage::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "operation" && value == "compare commit trees")
        );
    }

    #[test]
    fn test_open_refuses_a_workspace_without_version_control() {
        let directory = tempfile::tempdir().expect("temp dir");
        let error = Repository::open(directory.path()).expect_err("no repository");
        assert_eq!(error.slug(), errors::history::unversioned::SLUG);
        let canonical = fs::canonicalize(directory.path()).expect("canonical root");
        assert!(
            error
                .context()
                .any(|(key, value)| key == "workspace" && value == canonical.display().to_string())
        );
        assert!(
            error
                .context()
                .any(|(key, value)| key == "requires" && value.contains("git init"))
        );
    }

    #[test]
    fn test_open_refuses_a_missing_root_as_storage() {
        let error =
            Repository::open(Path::new("missing-rift-history-root")).expect_err("missing root");
        assert_eq!(error.slug(), errors::history::storage::SLUG);
    }

    #[test]
    fn test_open_finds_the_repository_that_versions_the_workspace() {
        let directory = repository_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        assert_eq!(
            repository.root(),
            fs::canonicalize(directory.path()).expect("canonical root")
        );
    }

    #[test]
    fn test_resolve_accepts_branch_tag_and_commit_spellings() {
        let directory = repository_fixture();
        git(directory.path(), &["tag", "v-probe"]);
        let repository = Repository::open(directory.path()).expect("repository");
        let by_branch = repository.resolve("main").expect("branch resolves");
        let by_tag = repository.resolve("v-probe").expect("tag resolves");
        let full = by_branch.commit_id();
        assert_eq!(full.len(), 40, "commit id must be the full hex spelling");
        let by_commit = repository.resolve(&full).expect("commit id resolves");
        let by_short = repository.resolve(&full[..8]).expect("short id resolves");
        for resolved in [&by_tag, &by_commit, &by_short] {
            assert_eq!(resolved.commit_id(), full);
        }
    }

    #[test]
    fn test_resolve_refuses_an_unknown_revision() {
        let directory = repository_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let error = repository
            .resolve("feature/absent")
            .expect_err("unknown revision");
        assert_eq!(error.slug(), errors::history::revision_unknown::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "rev" && value == "feature/absent")
        );
        assert!(error.to_string().contains("feature/absent"));
    }

    #[test]
    fn test_resolve_refuses_a_revision_naming_a_non_commit() {
        let directory = repository_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let error = repository
            .resolve("main^{tree}")
            .expect_err("a tree is not a commit");
        assert_eq!(error.slug(), errors::history::revision_not_commit::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "resolved_kind" && value == "tree")
        );
    }

    #[test]
    fn test_tree_files_lists_committed_files_sorted_and_filtered() {
        let directory = repository_fixture();
        fs::create_dir_all(directory.path().join("src")).expect("directory");
        fs::write(directory.path().join("src/extra.rs"), "pub fn extra() {}\n").expect("source");
        fs::write(directory.path().join("README.md"), "prose\n").expect("prose");
        commit_all(directory.path(), "add extra");
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let all: Vec<String> = repository
            .tree_files(&head, &include_all, REVISION_TREE_ENTRIES_MAX)
            .expect("listing")
            .iter()
            .map(|file| file.path().to_owned())
            .collect();
        assert_eq!(all, ["README.md", "lib.rs", "src/extra.rs"]);
        let rust_only: Vec<String> = repository
            .tree_files(
                &head,
                &|path| {
                    Path::new(path)
                        .extension()
                        .is_some_and(|extension| extension == "rs")
                },
                REVISION_TREE_ENTRIES_MAX,
            )
            .expect("filtered listing")
            .iter()
            .map(|file| file.path().to_owned())
            .collect();
        assert_eq!(rust_only, ["lib.rs", "src/extra.rs"]);
    }

    #[test]
    fn test_tree_files_serves_the_committed_state_not_the_working_tree() {
        let directory = repository_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("main").expect("head resolves");
        fs::write(
            directory.path().join("uncommitted.rs"),
            "pub fn later() {}\n",
        )
        .expect("uncommitted source");
        let listed: Vec<String> = repository
            .tree_files(&head, &include_all, REVISION_TREE_ENTRIES_MAX)
            .expect("listing")
            .iter()
            .map(|file| file.path().to_owned())
            .collect();
        assert_eq!(
            listed,
            ["lib.rs"],
            "an uncommitted file is not in the revision"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_tree_files_skips_symbolic_links() {
        let directory = repository_fixture();
        std::os::unix::fs::symlink("lib.rs", directory.path().join("alias.rs")).expect("symlink");
        commit_all(directory.path(), "add symlink");
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let listed: Vec<String> = repository
            .tree_files(&head, &include_all, REVISION_TREE_ENTRIES_MAX)
            .expect("listing")
            .iter()
            .map(|file| file.path().to_owned())
            .collect();
        assert_eq!(listed, ["lib.rs"], "a symbolic link is never listed");
    }

    #[test]
    fn test_workspace_below_the_repository_root_is_re_relativized() {
        let directory = tempfile::tempdir().expect("temp dir");
        init(directory.path());
        let workspace = directory.path().join("packages/app");
        fs::create_dir_all(&workspace).expect("workspace directory");
        fs::write(workspace.join("lib.rs"), "pub fn nested() {}\n").expect("source");
        fs::write(directory.path().join("outside.rs"), "pub fn outside() {}\n").expect("source");
        commit_all(directory.path(), "introduce nested workspace");
        let repository = Repository::open(&workspace).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let listed: Vec<String> = repository
            .tree_files(&head, &include_all, REVISION_TREE_ENTRIES_MAX)
            .expect("listing")
            .iter()
            .map(|file| file.path().to_owned())
            .collect();
        assert_eq!(
            listed,
            ["lib.rs"],
            "only entries below the workspace are listed, workspace-relative"
        );
    }

    #[test]
    fn test_tree_files_refuses_a_tree_over_the_entry_bound() {
        let directory = repository_fixture();
        fs::create_dir_all(directory.path().join("src")).expect("directory");
        fs::write(directory.path().join("src/extra.rs"), "pub fn extra() {}\n").expect("source");
        fs::write(directory.path().join("src/more.rs"), "pub fn more() {}\n").expect("source");
        commit_all(directory.path(), "grow the tree");
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let error = repository
            .tree_files(&head, &include_all, 2)
            .expect_err("four entries must refuse a two-entry budget");
        assert_eq!(error.slug(), errors::history::tree_too_large::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "entries_max" && value == "2")
        );
        let exactly_enough = repository
            .tree_files(&head, &include_all, 4)
            .expect("a budget covering every entry lists the tree");
        assert_eq!(exactly_enough.len(), 3, "three files beside one directory");
    }

    #[test]
    fn test_blob_bytes_reads_content_and_refuses_oversized_blobs() {
        let directory = repository_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let files = repository
            .tree_files(&head, &include_all, REVISION_TREE_ENTRIES_MAX)
            .expect("listing");
        let bytes = repository
            .blob_bytes(&files[0], 4_096)
            .expect("bytes within bound");
        assert_eq!(bytes, b"pub fn beacon() {}\n");
        let error = repository
            .blob_bytes(&files[0], 4)
            .expect_err("blob over the byte bound");
        assert_eq!(error.slug(), errors::history::blob_too_large::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "size" && value == "19")
        );
    }

    #[test]
    fn test_unborn_repository_refuses_head_as_unknown() {
        let directory = tempfile::tempdir().expect("temp dir");
        init(directory.path());
        let repository = Repository::open(directory.path()).expect("repository");
        let error = repository.resolve("HEAD").expect_err("unborn branch");
        assert_eq!(error.slug(), errors::history::revision_unknown::SLUG);
    }

    #[test]
    fn test_every_history_error_renders_its_registered_identity_and_evidence() {
        let cases = [
            (
                errors::history::unversioned()
                    .workspace(Path::new("/workspace"))
                    .requires("run git init")
                    .error(),
                errors::history::unversioned::SLUG,
                "workspace",
            ),
            (
                errors::history::revision_unknown()
                    .rev("feature/absent")
                    .requires("use a known revision")
                    .error(),
                errors::history::revision_unknown::SLUG,
                "rev",
            ),
            (
                errors::history::revision_not_commit()
                    .rev("main^{tree}")
                    .resolved_kind("tree")
                    .requires("use a commit")
                    .error(),
                errors::history::revision_not_commit::SLUG,
                "resolved_kind",
            ),
            (
                errors::history::tree_too_large()
                    .limit("revision_tree_entries_max")
                    .entries_max(4_usize)
                    .error(),
                errors::history::tree_too_large::SLUG,
                "entries_max",
            ),
            (
                errors::history::blob_too_large()
                    .path(Path::new("src/lib.rs"))
                    .bytes_max(4_usize)
                    .size(19_u64)
                    .error(),
                errors::history::blob_too_large::SLUG,
                "bytes_max",
            ),
            (
                errors::history::too_many_tags()
                    .limit("tags_max")
                    .tags_max(4_usize)
                    .error(),
                errors::history::too_many_tags::SLUG,
                "tags_max",
            ),
            (
                errors::history::path_unrepresentable()
                    .path("src/evil")
                    .error(),
                errors::history::path_unrepresentable::SLUG,
                "path",
            ),
            (
                errors::history::storage()
                    .operation("read blob")
                    .detail("object store gone")
                    .error(),
                errors::history::storage::SLUG,
                "operation",
            ),
        ];
        for (error, slug, evidence_key) in cases {
            assert_eq!(error.slug(), slug);
            assert!(
                error.context().any(|(key, _)| key == evidence_key),
                "{slug} must carry {evidence_key} evidence: {error}"
            );
        }
    }

    #[test]
    fn test_tree_files_stops_descending_once_the_budget_is_spent() {
        let directory = repository_fixture();
        fs::create_dir_all(directory.path().join("src")).expect("directory");
        fs::write(directory.path().join("src/extra.rs"), "pub fn extra() {}\n").expect("source");
        commit_all(directory.path(), "grow below a directory");
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        // Budget 1: the root's first entry spends it, so the `src` subtree
        // is refused before the walk ever descends into it.
        let error = repository
            .tree_files(&head, &include_all, 1)
            .expect_err("a subtree past the budget must refuse, not descend");
        assert_eq!(error.slug(), errors::history::tree_too_large::SLUG);
    }

    /// A workspace whose lib.rs changed in three commits, with one commit
    /// in between touching only another file.
    fn walked_fixture() -> tempfile::TempDir {
        let directory = repository_fixture();
        fs::write(directory.path().join("other.rs"), "pub fn other() {}\n").expect("source");
        commit_all(directory.path(), "introduce other");
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() -> u8 { 7 }\n",
        )
        .expect("source");
        commit_all(directory.path(), "widen beacon");
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() -> u8 { 9 }\n",
        )
        .expect("source");
        commit_all(directory.path(), "raise beacon");
        directory
    }

    #[test]
    fn test_path_revisions_lists_touching_commits_newest_first() {
        let directory = walked_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let history = repository
            .path_revisions(&head, "lib.rs", 100)
            .expect("walk");
        assert!(history.is_complete(), "four commits fit a bound of 100");
        let summaries: Vec<Option<&str>> = history
            .revisions()
            .iter()
            .map(PathRevision::summary)
            .collect();
        assert_eq!(
            summaries,
            [
                Some("raise beacon"),
                Some("widen beacon"),
                Some("introduce beacon")
            ],
            "the commit touching only other.rs is not listed"
        );
        for revision in history.revisions() {
            assert_eq!(
                revision.commit_id().len(),
                40,
                "a commit id is the full hex spelling"
            );
            assert_eq!(revision.timestamp(), "2026-01-01T00:00:00+00:00");
        }
        let newest = history.revisions()[0].blob().expect("blob present");
        assert_eq!(newest.path(), "lib.rs");
        let bytes = repository.blob_bytes(newest, 4_096).expect("bytes");
        assert_eq!(bytes, b"pub fn beacon() -> u8 { 9 }\n");
        let oldest = history.revisions()[2].blob().expect("blob present");
        assert_ne!(
            newest.blob_id(),
            oldest.blob_id(),
            "each touching commit carries its own committed bytes"
        );
    }

    #[test]
    fn test_path_revisions_of_an_uncommitted_path_is_empty_and_complete() {
        let directory = repository_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let history = repository
            .path_revisions(&head, "never.rs", 100)
            .expect("walk");
        assert_eq!(history.revisions(), []);
        assert!(history.is_complete());
    }

    #[test]
    fn test_path_revisions_bound_exactly_covering_history_is_complete() {
        let directory = walked_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let exact = repository.path_revisions(&head, "lib.rs", 4).expect("walk");
        assert!(
            exact.is_complete(),
            "a bound equal to the commit count reaches the first commit"
        );
        assert_eq!(exact.revisions().len(), 3);
    }

    #[test]
    fn test_path_revisions_one_under_the_history_stops_incomplete() {
        let directory = walked_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let truncated = repository.path_revisions(&head, "lib.rs", 3).expect("walk");
        assert!(
            !truncated.is_complete(),
            "a bound under the commit count cannot prove the walk covered the history"
        );
        let summaries: Vec<Option<&str>> = truncated
            .revisions()
            .iter()
            .map(PathRevision::summary)
            .collect();
        assert_eq!(
            summaries,
            [Some("raise beacon"), Some("widen beacon")],
            "only the newest touching commits inside the bound are listed"
        );
    }

    #[test]
    fn test_path_revisions_zero_bound_examines_nothing() {
        let directory = walked_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let none = repository.path_revisions(&head, "lib.rs", 0).expect("walk");
        assert_eq!(none.revisions(), []);
        assert!(!none.is_complete());
    }

    #[test]
    fn test_path_revisions_removed_path_carries_no_blob() {
        let directory = repository_fixture();
        git(directory.path(), &["rm", "-q", "lib.rs"]);
        commit_all(directory.path(), "retire beacon");
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let history = repository
            .path_revisions(&head, "lib.rs", 100)
            .expect("walk");
        assert_eq!(history.revisions().len(), 2);
        assert!(
            history.revisions()[0].blob().is_none(),
            "the removing commit holds no blob at the path"
        );
        assert!(history.revisions()[1].blob().is_some());
    }

    #[test]
    fn test_path_revisions_follows_first_parents_only() {
        let directory = repository_fixture();
        git(directory.path(), &["checkout", "-q", "-b", "side"]);
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() { /* side */ }\n",
        )
        .expect("source");
        commit_all(directory.path(), "side edit");
        git(directory.path(), &["checkout", "-q", "main"]);
        git(
            directory.path(),
            &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
        );
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("main").expect("head resolves");
        let history = repository
            .path_revisions(&head, "lib.rs", 100)
            .expect("walk");
        let summaries: Vec<Option<&str>> = history
            .revisions()
            .iter()
            .map(PathRevision::summary)
            .collect();
        assert_eq!(
            summaries,
            [Some("merge side"), Some("introduce beacon")],
            "the merge commit changed the entry against its first parent; \
             the side-branch commit stays invisible"
        );
        assert!(history.is_complete());
    }

    #[test]
    fn test_path_revisions_summary_is_the_message_summary_line() {
        let directory = repository_fixture();
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() { 0; }\n").expect("source");
        commit_all(
            directory.path(),
            "summary line\n\nbody paragraph the summary never carries",
        );
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let history = repository
            .path_revisions(&head, "lib.rs", 100)
            .expect("walk");
        assert_eq!(history.revisions()[0].summary(), Some("summary line"));
    }

    #[test]
    fn test_path_revisions_workspace_below_the_repository_root_uses_the_prefix() {
        let directory = tempfile::tempdir().expect("temp dir");
        init(directory.path());
        let workspace = directory.path().join("packages/app");
        fs::create_dir_all(&workspace).expect("workspace directory");
        fs::write(workspace.join("lib.rs"), "pub fn nested() {}\n").expect("source");
        commit_all(directory.path(), "introduce nested workspace");
        let repository = Repository::open(&workspace).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let history = repository
            .path_revisions(&head, "lib.rs", 100)
            .expect("walk");
        assert_eq!(history.revisions().len(), 1);
        assert_eq!(
            history.revisions()[0].blob().expect("blob present").path(),
            "lib.rs",
            "the workspace-relative spelling answers, not the repository one"
        );
    }

    /// Three commits touching lib.rs, with `.git/shallow` naming the second
    /// the way `git clone --depth` records the boundary; returns the first
    /// commit's id, the one past that boundary.
    fn shallow_fixture() -> (tempfile::TempDir, String) {
        let directory = repository_fixture();
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() -> u8 { 7 }\n",
        )
        .expect("source");
        commit_all(directory.path(), "widen beacon");
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() -> u8 { 9 }\n",
        )
        .expect("source");
        commit_all(directory.path(), "raise beacon");
        let repository = Repository::open(directory.path()).expect("repository");
        let second = repository.resolve("HEAD~1").expect("second commit");
        let first = repository.resolve("HEAD~2").expect("first commit");
        fs::write(
            directory.path().join(".git/shallow"),
            format!("{}\n", second.commit_id()),
        )
        .expect("shallow file");
        (directory, first.commit_id())
    }

    /// The loose object file holding `commit_id`.
    fn loose_object(root: &Path, commit_id: &str) -> PathBuf {
        root.join(".git/objects")
            .join(&commit_id[..2])
            .join(&commit_id[2..])
    }

    #[test]
    fn test_path_revisions_ends_incomplete_at_the_shallow_boundary() {
        let (directory, _first) = shallow_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let history = repository
            .path_revisions(&head, "lib.rs", 100)
            .expect("walk");
        let summaries: Vec<Option<&str>> = history
            .revisions()
            .iter()
            .map(PathRevision::summary)
            .collect();
        assert_eq!(
            summaries,
            [Some("raise beacon"), Some("widen beacon")],
            "the commit the shallow file names is the last one examined"
        );
        assert!(
            !history.is_complete(),
            "a shallow boundary is not the path's first commit"
        );
    }

    #[test]
    fn test_path_revisions_never_reads_past_the_shallow_boundary() {
        let (directory, first) = shallow_fixture();
        fs::remove_file(loose_object(directory.path(), &first))
            .expect("the first commit's object is a loose file");
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let history = repository
            .path_revisions(&head, "lib.rs", 100)
            .expect("the walk never asks for the boundary's parent");
        assert_eq!(history.revisions().len(), 2);
        assert!(!history.is_complete());
    }

    #[test]
    fn test_path_revisions_refuses_a_missing_parent_outside_the_shallow_set() {
        let (directory, first) = shallow_fixture();
        fs::remove_file(directory.path().join(".git/shallow")).expect("shallow file");
        fs::remove_file(loose_object(directory.path(), &first))
            .expect("the first commit's object is a loose file");
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let error = repository
            .path_revisions(&head, "lib.rs", 100)
            .expect_err("a parent absent without a shallow boundary is a storage failure");
        assert_eq!(error.slug(), errors::history::storage::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "operation" && value == "read commit")
        );
    }

    #[test]
    fn test_tree_files_refuses_a_non_utf8_committed_path() {
        let directory = repository_fixture();
        crate::fixture::commit_raw_path(directory.path(), b"evil-\xff.rs", "refs/heads/raw");
        let repository = Repository::open(directory.path()).expect("repository");
        let raw = repository.resolve("raw").expect("raw branch resolves");
        let error = repository
            .tree_files(&raw, &include_all, REVISION_TREE_ENTRIES_MAX)
            .expect_err("a committed non-UTF-8 path must refuse");
        assert_eq!(error.slug(), errors::history::path_unrepresentable::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "path" && value.contains("evil"))
        );
    }
}
