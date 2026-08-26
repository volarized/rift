//! What a change writes: one file's rewrite, the kind of write it is, and
//! the regions of the file's previous image it replaces.
//!
//! Every lane that writes - the patch engine, the rename and move planners,
//! the file-target insert - resolves to these values first, and the change
//! lane stages, publishes, and reports them.

use rift_core::ProjectPath;
use rift_syntax::ByteRange;

/// One region of a file's previous image a rewrite replaces, and the text
/// standing in it once the rewrite lands.
#[derive(Debug, Clone)]
pub(crate) struct ReplacedRegion {
    /// The replaced bytes, as offsets into the file's previous image.
    pub(crate) range: ByteRange,
    /// What stands in that range afterwards. Empty text deletes the region.
    pub(crate) text: String,
}

/// How one resolved rewrite changes the tree.
#[derive(Debug)]
pub(crate) enum RewriteKind {
    /// An existing file's content changes in place.
    ///
    /// Each region names the bytes of the previous image that one located
    /// hunk, or one engine-proposed edit, replaces. Regions are ascending
    /// and never overlap: a hunk only matches lines no earlier hunk wrote,
    /// and an engine proposal whose edits overlap refuses while planning.
    Modify {
        /// The previous image's regions this rewrite replaces.
        replaced: Vec<ReplacedRegion>,
    },
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

/// One file-level effect a change resolved to, not yet written.
#[derive(Debug)]
pub(crate) struct FileRewrite {
    /// The file this rewrite lands on.
    pub(crate) path: ProjectPath,
    /// What kind of write it is, and for a modification, what it replaces.
    pub(crate) kind: RewriteKind,
    /// The previous image's length in bytes. It is the span a create or a
    /// delete reports, and the span a modification falls back to when one
    /// batch's regions outnumber what a change result may carry.
    pub(crate) previous_len: u64,
    /// The file's whole content after this rewrite lands.
    pub(crate) next_source: String,
}

impl FileRewrite {
    /// Builds the in-place modification of `path` from its previous image
    /// to `next_source`, replacing `replaced`.
    pub(crate) fn modify(
        path: ProjectPath,
        previous: &str,
        next_source: String,
        replaced: Vec<ReplacedRegion>,
    ) -> Self {
        Self {
            path,
            kind: RewriteKind::Modify { replaced },
            previous_len: previous.len() as u64,
            next_source,
        }
    }

    /// Builds the creation of `path` holding `next_source`.
    pub(crate) const fn create(path: ProjectPath, next_source: String) -> Self {
        Self {
            path,
            kind: RewriteKind::Create,
            previous_len: 0,
            next_source,
        }
    }

    /// Builds the removal of `path`, whose previous image was `previous`.
    pub(crate) fn delete(path: ProjectPath, previous: &str) -> Self {
        Self {
            path,
            kind: RewriteKind::Delete,
            previous_len: previous.len() as u64,
            next_source: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use rift_syntax::ByteRange;

    use super::{FileRewrite, ReplacedRegion, RewriteKind};

    fn path(value: &str) -> rift_core::ProjectPath {
        rift_core::ProjectPath::new(value).expect("test path must be legal")
    }

    #[test]
    fn test_modify_records_its_previous_length_and_regions() {
        let rewrite = FileRewrite::modify(
            path("lib.rs"),
            "one\ntwo\n",
            "one\nTWO\n".to_owned(),
            vec![ReplacedRegion {
                range: ByteRange { start: 4, end: 8 },
                text: "TWO\n".to_owned(),
            }],
        );
        assert_eq!(rewrite.previous_len, 8);
        let RewriteKind::Modify { replaced } = &rewrite.kind else {
            panic!("a modification must carry its replaced regions");
        };
        assert_eq!(replaced.len(), 1);
        assert!(!rewrite.kind.removes_file());
    }

    #[test]
    fn test_create_starts_from_an_empty_previous_image() {
        let rewrite = FileRewrite::create(path("new.rs"), "body\n".to_owned());
        assert_eq!(rewrite.previous_len, 0);
        assert!(matches!(rewrite.kind, RewriteKind::Create));
    }

    #[test]
    fn test_delete_spans_the_previous_image_and_leaves_nothing() {
        let rewrite = FileRewrite::delete(path("gone.rs"), "one\ntwo\n");
        assert_eq!(rewrite.previous_len, 8);
        assert!(rewrite.next_source.is_empty());
        assert!(rewrite.kind.removes_file());
    }
}
