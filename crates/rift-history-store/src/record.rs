//! What one analyzed commit writes into the store.

use std::collections::HashMap;

use rift_protocol::read::{CommitAuthor, SymbolVersionKind};

/// One path a commit changed against the commit it was compared with. A side
/// without a blob is an addition or a deletion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
    /// The workspace-relative path, forward-slash separated.
    pub path: String,
    /// The blob id the compared commit held at the path; absent for an
    /// addition.
    pub old_blob: Option<String>,
    /// The blob id the commit holds at the path; absent for a deletion.
    pub new_blob: Option<String>,
}

impl ChangedPath {
    /// The pure renames among `paths`, sorted by old path: a deletion and an
    /// addition pair when exactly one deletion and one addition carry the
    /// same blob id. Any other blob id - two additions of one empty file, say -
    /// pairs nothing, since no evidence says which addition answers which
    /// deletion.
    #[must_use]
    pub fn pure_renames(paths: &[Self]) -> Vec<RenamedPath> {
        let mut deleted: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut added: HashMap<&str, Vec<&str>> = HashMap::new();
        for changed in paths {
            match (&changed.old_blob, &changed.new_blob) {
                (Some(old), None) => deleted.entry(old).or_default().push(&changed.path),
                (None, Some(new)) => added.entry(new).or_default().push(&changed.path),
                _ => {}
            }
        }
        let mut renames: Vec<RenamedPath> = deleted
            .iter()
            .filter_map(|(blob, olds)| {
                match (olds.as_slice(), added.get(blob).map(Vec::as_slice)) {
                    ([old], Some([new])) => Some(RenamedPath {
                        old_path: (*old).to_owned(),
                        new_path: (*new).to_owned(),
                    }),
                    _ => None,
                }
            })
            .collect();
        renames.sort_by(|left, right| left.old_path.cmp(&right.old_path));
        renames
    }
}

/// One file a commit moved without changing its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenamedPath {
    /// The path the compared commit held the file at.
    pub old_path: String,
    /// The path the commit holds the file at.
    pub new_path: String,
}

/// One declaration a commit moved to another file with its bytes unchanged, while the
/// file itself changed: the declaration's only match among the commit's deleted files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovedDeclaration {
    /// The path the declaration lived at before the commit.
    pub old_path: String,
    /// The path the commit moved it to.
    pub new_path: String,
    /// The provider's qualified name for the declaration.
    pub qualified_name: String,
}

/// One declaration whose shape changed on one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclarationChange {
    /// The path the declaration lives at in the commit, or lived at when the
    /// commit removed it.
    pub path: String,
    /// The provider's qualified name for the declaration.
    pub qualified_name: String,
    /// How the declaration changed.
    pub change: SymbolVersionKind,
}

/// One analyzed commit, built before the write transaction opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRecord {
    /// The commit id, full lowercase hex.
    pub id: String,
    /// The commit this one was compared with: its first parent, or the
    /// release before it. Absent when the commit was compared with nothing.
    pub base: Option<String>,
    /// Whether history past this commit is out of the store's reach: a
    /// shallow clone's boundary, or a commit compared with nothing although
    /// it has a parent.
    pub boundary: bool,
    /// The author the commit records.
    pub author: CommitAuthor,
    /// The committer time, an RFC 3339 date-time carrying the recorded
    /// offset.
    pub committed_at: String,
    /// The committer time in seconds since the Unix epoch, which orders
    /// commits across offsets.
    pub time: i64,
    /// The full commit message.
    pub message: String,
    /// The paths the commit changed.
    pub paths: Vec<ChangedPath>,
    /// The pure renames among [`Self::paths`].
    pub renames: Vec<RenamedPath>,
    /// The declarations moved between files the pure renames leave unpaired.
    pub moves: Vec<MovedDeclaration>,
    /// The declarations whose shape changed.
    pub declarations: Vec<DeclarationChange>,
}
