//! What a change writes: one file's rewrite, the kind of write it is, and
//! the regions of the file's previous image it replaces.
//!
//! Every lane that writes - the patch engine, the rename and move planners,
//! the file-target insert - resolves to these values first, and the change
//! lane stages, publishes, and reports them.

use std::fs;

use rift_core::ProjectPath;
use rift_syntax::ByteRange;

/// Longest a rewrite's resulting file may hold, in UTF-8 bytes.
/// [`crate::publish::publish_rewrites`] checks every rewrite's `next_source` against
/// this bound before staging, so a create edit, a whole-file report past
/// `CHANGE_EDITS_MAX`, and a small patch against an already oversized file all refuse
/// the same way. Equal in value to [`rift_protocol::change::BODY_BYTES_MAX`], pinned by
/// a conformance test in this module - the two are enforced at different points (a
/// request's advertised body length, and a write's resulting file length) and so are
/// declared separately, never aliased.
pub(crate) const REWRITE_FILE_BYTES_MAX: usize = 1_048_576;

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
#[derive(Clone, Debug)]
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
    /// What kind of write it is, and for a modification, what it replaces.
    pub(crate) kind: RewriteKind,
    /// The file's whole content before this rewrite lands. Empty for a creation.
    pub(crate) previous_source: String,
    /// The file's whole content after this rewrite lands.
    pub(crate) next_source: String,
    /// How publication selects file permissions.
    pub(crate) permissions: RewritePermissions,
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

    /// Length of file before rewrite.
    pub(crate) const fn previous_len(&self) -> u64 {
        self.previous_source.len() as u64
    }

    /// Whether rewrite changes file bytes.
    pub(crate) fn changes_bytes(&self) -> bool {
        !matches!(self.kind, RewriteKind::Modify { .. }) || self.previous_source != self.next_source
    }
}

#[cfg(test)]
mod tests {
    use rift_syntax::ByteRange;
    use schemars::schema_for;
    use serde_json::json;

    use super::{FileRewrite, REWRITE_FILE_BYTES_MAX, ReplacedRegion, RewriteKind};

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
        assert_eq!(rewrite.previous_len(), 8);
        let RewriteKind::Modify { replaced } = &rewrite.kind else {
            panic!("a modification must carry its replaced regions");
        };
        assert_eq!(replaced.len(), 1);
        assert!(!rewrite.kind.removes_file());
    }

    #[test]
    fn test_create_starts_from_an_empty_previous_image() {
        let rewrite = FileRewrite::create(path("new.rs"), "body\n".to_owned());
        assert_eq!(rewrite.previous_len(), 0);
        assert!(matches!(rewrite.kind, RewriteKind::Create));
    }

    #[test]
    fn test_delete_spans_the_previous_image_and_leaves_nothing() {
        let rewrite = FileRewrite::delete(path("gone.rs"), "one\ntwo\n");
        assert_eq!(rewrite.previous_len(), 8);
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

    /// `Edit::Replace.text`'s schema literal restates `REWRITE_FILE_BYTES_MAX`,
    /// because attribute arguments take only literals; this pins the two together.
    #[test]
    fn test_edit_replace_text_schema_length_pins_the_enforced_bound() {
        let schema =
            serde_json::to_value(schema_for!(rift_protocol::change::Edit)).expect("schema");
        let replace = schema["oneOf"]
            .as_array()
            .expect("Edit is a tagged union")
            .iter()
            .find(|variant| variant["properties"]["kind"]["const"] == json!("replace"))
            .expect("Edit carries a replace variant");
        assert_eq!(
            replace["properties"]["text"]["maxLength"],
            json!(REWRITE_FILE_BYTES_MAX),
            "the advertised length must equal the enforced constant"
        );
    }
}
