//! Which visible files one workspace publication replaces.
//!
//! A filesystem event names paths, and a change tool knows the paths it wrote. Both
//! arrive here as observations, and this module turns them into the value the workspace
//! index and the lexical index both consume: one [`ChangeSet`] naming exactly the files
//! whose bytes differ from the published ones.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use rift_core::ProjectPath;
use sha2::{Digest as _, Sha256};

/// One file's content identity: the SHA-256 of its bytes.
///
/// The workspace index carries this beside every file it holds, so resolving an
/// observation costs one hash of the observed bytes and one map lookup, never a
/// comparison of whole sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileDigest([u8; 32]);

impl FileDigest {
    /// Digests one file's bytes.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// The digest's bytes, as workspace identity material absorbs them.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// How one observed path differs from the publication it was compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathChange {
    /// The publication holds no file at this path and the filesystem now does.
    Added,
    /// Both hold a file at this path, and their bytes differ.
    Modified,
    /// The publication holds a file at this path and the filesystem no longer does,
    /// or the workspace's policy no longer includes it.
    Removed,
}

/// The observed paths one rebuild replaces, each classified exactly once.
///
/// Keying by path is what keeps the three classes disjoint: a path the resolution saw
/// twice keeps its first classification rather than landing in two sets whose readers
/// would then disagree about what to reparse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathChanges(BTreeMap<ProjectPath, PathChange>);

impl PathChanges {
    /// Whether no observed path differed from the publication.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many paths this rebuild replaces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Every classified path with its classification, in project-path order.
    pub fn iter(&self) -> impl Iterator<Item = (&ProjectPath, PathChange)> {
        self.0.iter().map(|(path, change)| (path, *change))
    }

    /// The paths whose current bytes the rebuild reads: added and modified alike.
    pub fn indexed(&self) -> impl Iterator<Item = &ProjectPath> {
        self.paths_where(|change| matches!(change, PathChange::Added | PathChange::Modified))
    }

    /// The paths the rebuild drops from the index: removed and modified alike, since a
    /// modified file's previous entries are replaced rather than merged.
    pub fn dropped(&self) -> impl Iterator<Item = &ProjectPath> {
        self.paths_where(|change| matches!(change, PathChange::Modified | PathChange::Removed))
    }

    fn paths_where(
        &self,
        accepts: impl Fn(PathChange) -> bool,
    ) -> impl Iterator<Item = &ProjectPath> {
        self.0
            .iter()
            .filter(move |(_, change)| accepts(**change))
            .map(|(path, _)| path)
    }

    /// Classifies `observed` against the digests `published` answers with.
    ///
    /// Each observation carries the path's current digest, or nothing when the path holds
    /// no file the workspace includes - a deleted file and one the policy stopped
    /// including are the same removal to the index. An observation whose digest equals the
    /// published one is dropped: an editor writing through a temporary file and renaming
    /// it, and a `touch`, both report paths whose bytes did not change, and neither is
    /// worth a reparse.
    ///
    /// Work is one lookup per observation, so the caller's own bound on how many paths it
    /// retains bounds this.
    pub fn resolve(
        observed: impl IntoIterator<Item = (ProjectPath, Option<FileDigest>)>,
        published: impl Fn(&ProjectPath) -> Option<FileDigest>,
    ) -> Self {
        let mut changes = Self::default();
        for (path, current) in observed {
            let Some(change) = classify(published(&path), current) else {
                continue;
            };
            changes.classify(path, change);
        }
        changes
    }

    /// Classifies one whole capture against one whole publication.
    ///
    /// Unlike [`Self::resolve`], which answers for the paths an observation named, this
    /// walks both sets: a path only the publication holds is removed, and a path only the
    /// capture holds is added. A request that captured the tree itself uses this, because
    /// its capture already read every visible file.
    #[must_use]
    pub fn between(published: &WorkspaceDigests, captured: &WorkspaceDigests) -> Self {
        let observed = captured
            .iter()
            .map(|(path, digest)| (path.clone(), Some(digest)))
            .chain(
                published
                    .iter()
                    .filter(|(path, _)| captured.get(path).is_none())
                    .map(|(path, _)| (path.clone(), None)),
            );
        Self::resolve(observed, |path| published.get(path))
    }

    /// Records one classification, keeping the first one a path received.
    fn classify(&mut self, path: ProjectPath, change: PathChange) {
        if let Entry::Vacant(entry) = self.0.entry(path) {
            entry.insert(change);
        }
    }
}

/// Every visible file's digest at one moment, in project-path order.
///
/// A request-time capture reads the whole tree anyway to decide whether the publication
/// still answers for it. Keeping what it read as digests rather than folding them away
/// lets the same capture name the files that moved, so a request that finds the tree ahead
/// of the publication asks for those files rather than for the whole workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceDigests(BTreeMap<ProjectPath, FileDigest>);

impl WorkspaceDigests {
    /// Collects one digest set from path and digest pairs.
    pub fn new(digests: impl IntoIterator<Item = (ProjectPath, FileDigest)>) -> Self {
        Self(digests.into_iter().collect())
    }

    /// The digest recorded at `path`, or nothing when no file was recorded there.
    #[must_use]
    pub fn get(&self, path: &ProjectPath) -> Option<FileDigest> {
        self.0.get(path).copied()
    }

    /// How many files this set records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether this set records no file at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every recorded path with its digest, in project-path order.
    pub fn iter(&self) -> impl Iterator<Item = (&ProjectPath, FileDigest)> {
        self.0.iter().map(|(path, digest)| (path, *digest))
    }
}

/// What one rebuild covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeSet {
    /// Only the named paths are read again; every other file is shared with the
    /// previous publication.
    Incremental(PathChanges),
    /// Every visible file is discovered and read again. Startup, watch failure, and a
    /// configuration change take this path, as does any observation that names no
    /// trustworthy path set.
    Full,
}

impl ChangeSet {
    /// Whether this rebuild replaces nothing, so the previous publication already answers
    /// for the observation that produced it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Incremental(changes) => changes.is_empty(),
            Self::Full => false,
        }
    }
}

/// How one path's published digest and its current one compare, or nothing when the
/// rebuild has no work for it.
fn classify(published: Option<FileDigest>, current: Option<FileDigest>) -> Option<PathChange> {
    match (published, current) {
        (None, Some(_)) => Some(PathChange::Added),
        (Some(_), None) => Some(PathChange::Removed),
        (Some(published), Some(current)) if published != current => Some(PathChange::Modified),
        (Some(_), Some(_)) | (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rift_core::ProjectPath;

    use super::{ChangeSet, FileDigest, PathChange, PathChanges};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn path(value: &str) -> TestResult<ProjectPath> {
        Ok(ProjectPath::new(value)?)
    }

    fn published(entries: &[(&str, &str)]) -> TestResult<BTreeMap<ProjectPath, FileDigest>> {
        entries
            .iter()
            .map(|(name, bytes)| Ok((path(name)?, FileDigest::of(bytes.as_bytes()))))
            .collect()
    }

    fn classified(changes: &PathChanges) -> Vec<(String, PathChange)> {
        changes
            .iter()
            .map(|(path, change)| (path.as_str().to_owned(), change))
            .collect()
    }

    #[test]
    fn test_resolve_classifies_added_modified_and_removed_paths() -> TestResult {
        let published = published(&[("src/kept.rs", "kept"), ("src/gone.rs", "gone")])?;
        let observed = vec![
            (path("src/new.rs")?, Some(FileDigest::of(b"new"))),
            (path("src/kept.rs")?, Some(FileDigest::of(b"edited"))),
            (path("src/gone.rs")?, None),
        ];
        let resolved = PathChanges::resolve(observed, |path| published.get(path).copied());
        assert_eq!(
            classified(&resolved),
            vec![
                ("src/gone.rs".to_owned(), PathChange::Removed),
                ("src/kept.rs".to_owned(), PathChange::Modified),
                ("src/new.rs".to_owned(), PathChange::Added),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_resolve_drops_a_path_whose_bytes_did_not_change() -> TestResult {
        let published = published(&[("src/lib.rs", "same")])?;
        let observed = vec![(path("src/lib.rs")?, Some(FileDigest::of(b"same")))];
        let resolved = PathChanges::resolve(observed, |path| published.get(path).copied());
        assert!(
            resolved.is_empty(),
            "an unchanged path leaves nothing to rebuild"
        );
        Ok(())
    }

    #[test]
    fn test_resolve_drops_a_path_absent_from_both_sides() -> TestResult {
        let observed = vec![(path("src/never.rs")?, None)];
        let resolved = PathChanges::resolve(observed, |_| None);
        assert!(resolved.is_empty(), "a path neither side holds is no work");
        Ok(())
    }

    #[test]
    fn test_resolve_keeps_one_classification_per_repeated_path() -> TestResult {
        let published = published(&[("src/lib.rs", "before")])?;
        let observed = vec![
            (path("src/lib.rs")?, Some(FileDigest::of(b"after"))),
            (path("src/lib.rs")?, None),
        ];
        let resolved = PathChanges::resolve(observed, |path| published.get(path).copied());
        assert_eq!(
            classified(&resolved),
            vec![("src/lib.rs".to_owned(), PathChange::Modified)]
        );
        Ok(())
    }

    #[test]
    fn test_indexed_and_dropped_split_the_classified_paths() -> TestResult {
        let published = published(&[("a.md", "a"), ("b.md", "b")])?;
        let observed = vec![
            (path("a.md")?, Some(FileDigest::of(b"a2"))),
            (path("b.md")?, None),
            (path("c.md")?, Some(FileDigest::of(b"c"))),
        ];
        let resolved = PathChanges::resolve(observed, |path| published.get(path).copied());
        let changes = &resolved;
        let indexed: Vec<_> = changes.indexed().map(ProjectPath::as_str).collect();
        let dropped: Vec<_> = changes.dropped().map(ProjectPath::as_str).collect();
        assert_eq!(indexed, vec!["a.md", "c.md"]);
        assert_eq!(dropped, vec!["a.md", "b.md"]);
        assert_eq!(changes.len(), 3);
        Ok(())
    }

    #[test]
    fn test_full_is_never_empty() {
        assert!(
            !ChangeSet::Full.is_empty(),
            "a full rebuild always has work"
        );
    }

    #[test]
    fn test_file_digest_separates_different_bytes() {
        assert_eq!(FileDigest::of(b"same"), FileDigest::of(b"same"));
        assert_ne!(FileDigest::of(b"one"), FileDigest::of(b"two"));
        assert_eq!(FileDigest::of(b"").as_bytes().len(), 32);
    }
}
