//! What a change writes: one file's rewrite, the kind of write it is, and
//! the file fact it reports once it lands.
//!
//! Every lane that writes - the patch engine, the rename and move planners,
//! the file-target insert - resolves to these values first, and the change
//! lane stages, publishes, and reports them.

use std::fs;

use rift_core::ProjectPath;
use rift_protocol::change::{FileChange, FileChangeKind};

/// Longest a rewrite's resulting file may hold, in UTF-8 bytes.
/// [`crate::publish::publish_rewrites`] checks every rewrite's `next_source` against
/// this bound before staging, so a creation and a small patch against an already
/// oversized file both refuse the same way. Equal in value to
/// [`rift_protocol::change::BODY_BYTES_MAX`], pinned by
/// a conformance test in this module - the two are enforced at different points (a
/// request's advertised body length, and a write's resulting file length) and so are
/// declared separately, never aliased.
pub(crate) const REWRITE_FILE_BYTES_MAX: usize = 1_048_576;

/// How one resolved rewrite changes the tree.
#[derive(Clone, Debug)]
pub(crate) enum RewriteKind {
    /// An existing file's content changes in place.
    Modify,
    /// A new file is written; its parent directories are created first.
    Create,
    /// An existing file is removed.
    Delete,
}

impl RewriteKind {
    /// Whether this rewrite removes its file.
    pub(crate) const fn removes_file(&self) -> bool {
        matches!(self, Self::Delete)
    }
}

/// Permission state one rewrite publishes.
#[derive(Clone, Debug)]
pub(crate) enum RewritePermissions {
    /// Retain permissions from existing target.
    Retain,
    /// Retain permissions from another workspace file.
    RetainFrom(ProjectPath),
    /// Publish captured permissions exactly.
    Exact(fs::Permissions),
}

/// One file-level effect a change resolved to, not yet written.
#[derive(Clone, Debug)]
pub(crate) struct FileRewrite {
    /// The file this rewrite lands on.
    pub(crate) path: ProjectPath,
    /// What kind of write it is: a modification, a creation, or a removal.
    pub(crate) kind: RewriteKind,
    /// The file's whole content before this rewrite lands. Empty for a creation.
    pub(crate) previous_source: String,
    /// The file's whole content after this rewrite lands.
    pub(crate) next_source: String,
    /// How publication selects file permissions.
    pub(crate) permissions: RewritePermissions,
}

/// One raw-byte whole-file rewrite used only to restore a rejected hook write.
#[derive(Clone, Debug)]
pub(crate) struct ByteFileRewrite {
    /// File this rewrite lands on.
    pub(crate) path: ProjectPath,
    /// Kind of filesystem change.
    pub(crate) kind: RewriteKind,
    /// Complete bytes after this rewrite lands.
    pub(crate) next_bytes: Vec<u8>,
    /// Permissions published with retained bytes.
    pub(crate) permissions: RewritePermissions,
}

impl ByteFileRewrite {
    /// Restores an existing file's bytes and permissions.
    pub(crate) fn modify(
        path: ProjectPath,
        next_bytes: Vec<u8>,
        permissions: fs::Permissions,
    ) -> Self {
        Self {
            path,
            kind: RewriteKind::Modify,
            next_bytes,
            permissions: RewritePermissions::Exact(permissions),
        }
    }

    /// Restores a missing file's bytes and permissions.
    pub(crate) fn create(
        path: ProjectPath,
        next_bytes: Vec<u8>,
        permissions: fs::Permissions,
    ) -> Self {
        Self {
            path,
            kind: RewriteKind::Create,
            next_bytes,
            permissions: RewritePermissions::Exact(permissions),
        }
    }

    /// Removes a file created by a rejected hook.
    pub(crate) fn delete(path: ProjectPath) -> Self {
        Self {
            path,
            kind: RewriteKind::Delete,
            next_bytes: Vec::new(),
            permissions: RewritePermissions::Retain,
        }
    }
}

impl FileRewrite {
    /// Builds the in-place modification of `path` from its previous image
    /// to `next_source`.
    pub(crate) fn modify(path: ProjectPath, previous: &str, next_source: String) -> Self {
        Self {
            path,
            kind: RewriteKind::Modify,
            previous_source: previous.to_owned(),
            next_source,
            permissions: RewritePermissions::Retain,
        }
    }

    /// Builds creation of `path` holding `next_source`.
    pub(crate) const fn create(path: ProjectPath, next_source: String) -> Self {
        Self {
            path,
            kind: RewriteKind::Create,
            previous_source: String::new(),
            next_source,
            permissions: RewritePermissions::Retain,
        }
    }

    /// Builds removal of `path`, whose previous image was `previous`.
    pub(crate) fn delete(path: ProjectPath, previous: &str) -> Self {
        Self {
            path,
            kind: RewriteKind::Delete,
            previous_source: previous.to_owned(),
            next_source: String::new(),
            permissions: RewritePermissions::Retain,
        }
    }

    /// Builds creation of `path`, retaining permissions from `permissions_from`.
    pub(crate) fn create_from(
        path: ProjectPath,
        next_source: String,
        permissions_from: ProjectPath,
    ) -> Self {
        Self {
            path,
            kind: RewriteKind::Create,
            previous_source: String::new(),
            next_source,
            permissions: RewritePermissions::RetainFrom(permissions_from),
        }
    }

    /// Sets exact permissions the published file receives.
    pub(crate) fn with_permissions(mut self, permissions: fs::Permissions) -> Self {
        self.permissions = RewritePermissions::Exact(permissions);
        self
    }

    /// Whether rewrite changes file bytes.
    pub(crate) fn changes_bytes(&self) -> bool {
        !matches!(self.kind, RewriteKind::Modify) || self.previous_source != self.next_source
    }

    /// The file fact this rewrite reports: what the write did to the file, and
    /// the file's size and line counts after it. Line counts come from diffing
    /// the two images, so a line whose content changed counts once as added and
    /// once as removed.
    pub(crate) fn change(&self) -> FileChange {
        let patch = diffy::create_patch(&self.previous_source, &self.next_source);
        let (lines_added, lines_removed) = patch.hunks().iter().flat_map(diffy::Hunk::lines).fold(
            (0_u64, 0_u64),
            |(added, removed), line| match line {
                diffy::Line::Insert(_) => (added + 1, removed),
                diffy::Line::Delete(_) => (added, removed + 1),
                diffy::Line::Context(_) => (added, removed),
            },
        );
        FileChange {
            path: rift_protocol::read::ProjectPath(self.path.as_str().to_owned()),
            kind: match self.kind {
                RewriteKind::Modify => FileChangeKind::Modified,
                RewriteKind::Create => FileChangeKind::Created,
                RewriteKind::Delete => FileChangeKind::Deleted,
            },
            size_bytes: self.next_source.len() as u64,
            line_count: rift_core::line::lines_inclusive(&self.next_source).count() as u64,
            lines_added,
            lines_removed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FileChangeKind, FileRewrite, REWRITE_FILE_BYTES_MAX, RewriteKind};

    fn path(value: &str) -> rift_core::ProjectPath {
        rift_core::ProjectPath::new(value).expect("test path must be legal")
    }

    #[test]
    fn test_modify_keeps_both_images_and_changes_bytes() {
        let rewrite = FileRewrite::modify(path("lib.rs"), "one\ntwo\n", "one\nTWO\n".to_owned());
        assert_eq!(rewrite.previous_source, "one\ntwo\n");
        assert!(matches!(rewrite.kind, RewriteKind::Modify));
        assert!(rewrite.changes_bytes());
        assert!(!rewrite.kind.removes_file());
    }

    #[test]
    fn test_create_starts_from_an_empty_previous_image() {
        let rewrite = FileRewrite::create(path("new.rs"), "body\n".to_owned());
        assert!(rewrite.previous_source.is_empty());
        assert!(matches!(rewrite.kind, RewriteKind::Create));
    }

    #[test]
    fn test_delete_keeps_the_previous_image_and_leaves_nothing() {
        let rewrite = FileRewrite::delete(path("gone.rs"), "one\ntwo\n");
        assert_eq!(rewrite.previous_source, "one\ntwo\n");
        assert!(rewrite.next_source.is_empty());
        assert!(rewrite.kind.removes_file());
    }

    /// `rift-protocol` cannot depend on `rift-server`, so
    /// [`rift_protocol::change::BODY_BYTES_MAX`] and `REWRITE_FILE_BYTES_MAX` are
    /// declared separately; this test keeps their values equal.
    #[test]
    fn test_rewrite_file_bytes_max_equals_the_advertised_body_bound() {
        assert_eq!(
            REWRITE_FILE_BYTES_MAX,
            rift_protocol::change::BODY_BYTES_MAX
        );
    }

    #[test]
    fn test_change_of_a_creation_counts_every_line_as_added() {
        let change = FileRewrite::create(path("new.rs"), "one\ntwo\n".to_owned()).change();
        assert_eq!(change.kind, FileChangeKind::Created);
        assert_eq!(change.size_bytes, 8);
        assert_eq!(change.line_count, 2);
        assert_eq!(change.lines_added, 2);
        assert_eq!(change.lines_removed, 0);
    }

    #[test]
    fn test_change_of_a_deletion_reports_no_bytes_and_the_lines_it_held() {
        let change = FileRewrite::delete(path("gone.rs"), "one\ntwo\n").change();
        assert_eq!(change.kind, FileChangeKind::Deleted);
        assert_eq!(change.size_bytes, 0);
        assert_eq!(change.line_count, 0);
        assert_eq!(change.lines_added, 0);
        assert_eq!(change.lines_removed, 2);
    }

    #[test]
    fn test_change_of_one_changed_line_counts_it_added_and_removed() {
        let change =
            FileRewrite::modify(path("lib.rs"), "one\ntwo\n", "one\nTWO\n".to_owned()).change();
        assert_eq!(change.kind, FileChangeKind::Modified);
        assert_eq!(change.size_bytes, 8);
        assert_eq!(change.line_count, 2);
        assert_eq!(change.lines_added, 1);
        assert_eq!(change.lines_removed, 1);
    }

    #[test]
    fn test_change_counts_a_final_line_carrying_no_newline() {
        let change = FileRewrite::create(path("new.rs"), "one\ntwo".to_owned()).change();
        assert_eq!(change.size_bytes, 7);
        assert_eq!(change.line_count, 2);
        assert_eq!(change.lines_added, 2);
        assert_eq!(change.lines_removed, 0);
    }

    #[test]
    fn test_change_counts_crlf_lines_the_way_it_counts_lf_lines() {
        let crlf = FileRewrite::modify(
            path("lib.rs"),
            "one\r\ntwo\r\n",
            "one\r\nTWO\r\n".to_owned(),
        )
        .change();
        let lf =
            FileRewrite::modify(path("lib.rs"), "one\ntwo\n", "one\nTWO\n".to_owned()).change();
        assert_eq!(
            (crlf.line_count, crlf.lines_added, crlf.lines_removed),
            (lf.line_count, lf.lines_added, lf.lines_removed),
            "a CRLF image must count the same lines as its LF twin"
        );
    }
}
