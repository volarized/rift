//! One workspace's git repository, opened in place for revision-addressed reads.

use std::path::{Path, PathBuf};

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault};

/// Tree entries one revision listing may visit, files and directories alike.
/// The traversal refuses a larger tree rather than truncating it silently.
pub const REVISION_TREE_ENTRIES_MAX: usize = 65_536;

/// One version-control access failure: what was asked of the repository, and
/// why it cannot be answered.
#[derive(Debug)]
pub enum HistoryFault {
    /// The workspace has no version-control repository to read revisions from.
    Unversioned {
        /// The workspace root that no repository versions.
        root: PathBuf,
    },
    /// The revision spelling does not resolve in the repository.
    RevisionUnknown {
        /// The spelling the request carried.
        rev: String,
    },
    /// The revision resolves to an object that is not a commit.
    RevisionNotCommit {
        /// The spelling the request carried.
        rev: String,
        /// The kind of object the spelling resolved to.
        kind: String,
    },
    /// The revision's tree holds more entries than the traversal bound.
    TreeTooLarge {
        /// The entry bound the traversal enforces.
        entries_max: usize,
    },
    /// One committed file is larger than the per-file byte bound.
    BlobTooLarge {
        /// The workspace-relative path of the oversized file.
        path: String,
        /// The per-file byte bound in force.
        bytes_max: usize,
        /// The committed file's actual size.
        size: u64,
    },
    /// A committed path inside the workspace is not valid UTF-8.
    PathUnrepresentable {
        /// The offending path, rendered lossily for the reader.
        path: String,
    },
    /// The repository's object store could not be read.
    Storage {
        /// The repository operation that failed.
        operation: &'static str,
        /// The rendered underlying failure.
        detail: String,
    },
}

impl Fault for HistoryFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::Unversioned { .. } => ErrorName::Wire(ErrorCode::CapabilityUnavailable),
            Self::RevisionUnknown { .. } | Self::RevisionNotCommit { .. } => {
                ErrorName::Wire(ErrorCode::ResourceNotFound)
            }
            Self::TreeTooLarge { .. } | Self::BlobTooLarge { .. } => {
                ErrorName::Wire(ErrorCode::LimitExceeded)
            }
            Self::PathUnrepresentable { .. } => ErrorName::Wire(ErrorCode::UnsupportedPath),
            Self::Storage { .. } => ErrorName::Wire(ErrorCode::StorageFailure),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::Unversioned { root } => vec![
                ErrorContext::new("workspace", root.display().to_string()),
                ErrorContext::new(
                    "requires",
                    "a git repository — run `git init`, or omit `rev` to read the current tree",
                ),
            ],
            Self::RevisionUnknown { rev } => vec![
                ErrorContext::new("rev", rev.clone()),
                ErrorContext::new(
                    "requires",
                    "a revision the repository resolves — a branch, tag, or commit id",
                ),
            ],
            Self::RevisionNotCommit { rev, kind } => vec![
                ErrorContext::new("rev", rev.clone()),
                ErrorContext::new("resolved_kind", kind.clone()),
                ErrorContext::new("requires", "a revision that names a commit"),
            ],
            Self::TreeTooLarge { entries_max } => vec![
                ErrorContext::new("limit", "revision_tree_entries_max"),
                ErrorContext::new("entries_max", entries_max.to_string()),
            ],
            Self::BlobTooLarge {
                path,
                bytes_max,
                size,
            } => vec![
                ErrorContext::new("path", path.clone()),
                ErrorContext::new("bytes_max", bytes_max.to_string()),
                ErrorContext::new("size", size.to_string()),
            ],
            Self::PathUnrepresentable { path } => vec![ErrorContext::new("path", path.clone())],
            Self::Storage { operation, detail } => vec![
                ErrorContext::new("operation", *operation),
                ErrorContext::new("detail", detail.clone()),
            ],
        }
    }
}

/// Opaque version-control access failure.
pub type HistoryError = Error<HistoryFault>;

/// One revision resolved to the commit it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRevision {
    commit: gix::ObjectId,
}

impl ResolvedRevision {
    /// The full lowercase hex commit id — the durable spelling the
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
    path: String,
    blob: gix::ObjectId,
}

impl TreeFile {
    /// The workspace-relative path, forward-slash separated.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// The git repository that versions one workspace root.
///
/// The workspace may sit below the repository's working-tree root; listings
/// then serve only entries inside the workspace, re-relativized to it.
#[derive(Debug)]
pub struct Repository {
    inner: gix::Repository,
    root: PathBuf,
    /// The workspace root's forward-slash path inside the repository's
    /// working tree; empty when the workspace is that root.
    prefix: String,
}

impl Repository {
    /// Opens the repository that versions `root`, discovering it upward the
    /// way git itself does.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError`] when no repository versions `root`, or when
    /// the filesystem cannot be read while discovering one.
    pub fn open(root: &Path) -> Result<Self, HistoryError> {
        let root = std::fs::canonicalize(root).map_err(|error| {
            storage("canonicalize workspace root", &error)
                .with_context(ErrorContext::new("workspace", root.display().to_string()))
        })?;
        let unversioned = || Error::new(HistoryFault::Unversioned { root: root.clone() });
        let inner = gix::discover(&root).map_err(|_| unversioned())?;
        let workdir = inner.workdir().ok_or_else(unversioned)?;
        let workdir = std::fs::canonicalize(workdir)
            .map_err(|error| storage("canonicalize repository root", &error))?;
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

    /// Resolves one revision spelling — a branch, tag, or commit id — to
    /// the commit it names.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError`] when the spelling resolves to nothing, or to
    /// an object that is not a commit.
    pub fn resolve(&self, rev: &str) -> Result<ResolvedRevision, HistoryError> {
        let unknown = || {
            Error::new(HistoryFault::RevisionUnknown {
                rev: rev.to_owned(),
            })
        };
        let id = self
            .inner
            .rev_parse_single(rev.as_bytes().as_bstr())
            .map_err(|_| unknown())?;
        let object = id.object().map_err(|_| unknown())?;
        let commit = object
            .peel_to_kind(gix::object::Kind::Commit)
            .map_err(|_| {
                Error::new(HistoryFault::RevisionNotCommit {
                    rev: rev.to_owned(),
                    kind: object_kind(&self.inner, id.detach()),
                })
            })?;
        Ok(ResolvedRevision { commit: commit.id })
    }

    /// Lists the revision's committed regular files inside the workspace
    /// whose workspace-relative path `admits` accepts, sorted by path.
    ///
    /// Symbolic links and submodules are never listed. The traversal counts
    /// every visited tree entry against `entries_max` — callers pass
    /// [`REVISION_TREE_ENTRIES_MAX`] — stops descending once the budget is
    /// spent, and refuses the listing rather than truncating it.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError`] for an unreadable object store, a committed
    /// non-UTF-8 path inside the workspace, or a tree over the entry bound.
    pub fn tree_files(
        &self,
        revision: &ResolvedRevision,
        admits: &dyn Fn(&str) -> bool,
        entries_max: usize,
    ) -> Result<Vec<TreeFile>, HistoryError> {
        let commit = self
            .inner
            .find_commit(revision.commit)
            .map_err(|error| storage("read commit", &error))?;
        let tree = commit
            .tree()
            .map_err(|error| storage("read commit tree", &error))?;
        let mut recorder = BoundedRecorder::new(entries_max);
        tree.traverse()
            .depthfirst(&mut recorder)
            .map_err(|error| storage("walk commit tree", &error))?;
        if recorder.exhausted {
            return Err(Error::new(HistoryFault::TreeTooLarge { entries_max }));
        }
        let mut files = Vec::new();
        for record in recorder.inner.records {
            if !record.mode.is_blob() {
                continue;
            }
            let Some(path) = self.workspace_relative(record.filepath.as_ref())? else {
                continue;
            };
            if !admits(&path) {
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
    /// Returns [`HistoryError`] for an oversized blob or an unreadable
    /// object store.
    pub fn blob_bytes(&self, file: &TreeFile, bytes_max: usize) -> Result<Vec<u8>, HistoryError> {
        let header = self
            .inner
            .find_header(file.blob)
            .map_err(|error| storage("read blob header", &error))?;
        if header.size() > bytes_max as u64 {
            return Err(Error::new(HistoryFault::BlobTooLarge {
                path: file.path.clone(),
                bytes_max,
                size: header.size(),
            }));
        }
        let object = self
            .inner
            .find_object(file.blob)
            .map_err(|error| storage("read blob", &error))?;
        Ok(object.detach().data)
    }

    /// The workspace-relative form of one repository-relative path: `None`
    /// when the path lies outside the workspace, an error when its bytes
    /// inside the workspace are not UTF-8.
    fn workspace_relative(&self, filepath: &[u8]) -> Result<Option<String>, HistoryError> {
        let Some(relative) = strip_workspace_prefix(filepath, self.prefix.as_bytes()) else {
            return Ok(None);
        };
        utf8_path(relative, filepath).map(Some)
    }
}

/// The bytes of `filepath` below the workspace prefix, or `None` for a path
/// outside the workspace. An empty prefix — the workspace at the repository
/// root — keeps every path.
fn strip_workspace_prefix<'a>(filepath: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if prefix.is_empty() {
        return Some(filepath);
    }
    filepath.strip_prefix(prefix)?.strip_prefix(b"/")
}

/// One committed path's UTF-8 form. The refusal renders the full
/// repository-relative spelling, so the reader sees the path git records.
fn utf8_path(relative: &[u8], filepath: &[u8]) -> Result<String, HistoryError> {
    std::str::from_utf8(relative)
        .map(str::to_owned)
        .map_err(|_| {
            Error::new(HistoryFault::PathUnrepresentable {
                path: String::from_utf8_lossy(filepath).into_owned(),
            })
        })
}

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

fn storage(operation: &'static str, error: &dyn std::fmt::Display) -> HistoryError {
    Error::new(HistoryFault::Storage {
        operation,
        detail: error.to_string(),
    })
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

    fn admit_all(_: &str) -> bool {
        true
    }

    #[test]
    fn test_open_refuses_a_workspace_without_version_control() {
        let directory = tempfile::tempdir().expect("temp dir");
        let error = Repository::open(directory.path()).expect_err("no repository");
        assert!(matches!(error.fault(), HistoryFault::Unversioned { .. }));
        let canonical = fs::canonicalize(directory.path()).expect("canonical root");
        assert_eq!(
            error.to_string(),
            format!(
                "no configured provider serves this request: workspace {}, \
                 requires a git repository — run `git init`, or omit `rev` to \
                 read the current tree; adjust the request to a served \
                 capability, or configure a provider that serves it",
                canonical.display()
            )
        );
    }

    #[test]
    fn test_open_refuses_a_missing_root_as_storage() {
        let error =
            Repository::open(Path::new("missing-rift-history-root")).expect_err("missing root");
        assert!(matches!(error.fault(), HistoryFault::Storage { .. }));
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
        assert!(matches!(
            error.fault(),
            HistoryFault::RevisionUnknown { .. }
        ));
        assert_eq!(
            error.to_string(),
            "the requested resource does not exist in the current snapshot: \
             rev feature/absent, requires a revision the repository resolves — \
             a branch, tag, or commit id; search or list first, then retry \
             with an identity that answer returned"
        );
    }

    #[test]
    fn test_resolve_refuses_a_revision_naming_a_non_commit() {
        let directory = repository_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let error = repository
            .resolve("main^{tree}")
            .expect_err("a tree is not a commit");
        let HistoryFault::RevisionNotCommit { kind, .. } = error.fault() else {
            panic!("expected RevisionNotCommit, got {:?}", error.fault());
        };
        assert_eq!(kind, "tree");
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
            .tree_files(&head, &admit_all, REVISION_TREE_ENTRIES_MAX)
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
            .tree_files(&head, &admit_all, REVISION_TREE_ENTRIES_MAX)
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
            .tree_files(&head, &admit_all, REVISION_TREE_ENTRIES_MAX)
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
            .tree_files(&head, &admit_all, REVISION_TREE_ENTRIES_MAX)
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
            .tree_files(&head, &admit_all, 2)
            .expect_err("four entries must refuse a two-entry budget");
        let HistoryFault::TreeTooLarge { entries_max } = error.fault() else {
            panic!("expected TreeTooLarge, got {:?}", error.fault());
        };
        assert_eq!(*entries_max, 2);
        let exactly_enough = repository
            .tree_files(&head, &admit_all, 4)
            .expect("a budget covering every entry lists the tree");
        assert_eq!(exactly_enough.len(), 3, "three files beside one directory");
    }

    #[test]
    fn test_blob_bytes_reads_content_and_refuses_oversized_blobs() {
        let directory = repository_fixture();
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head resolves");
        let files = repository
            .tree_files(&head, &admit_all, REVISION_TREE_ENTRIES_MAX)
            .expect("listing");
        let bytes = repository
            .blob_bytes(&files[0], 4_096)
            .expect("bytes within bound");
        assert_eq!(bytes, b"pub fn beacon() {}\n");
        let error = repository
            .blob_bytes(&files[0], 4)
            .expect_err("blob over the byte bound");
        let HistoryFault::BlobTooLarge { size, .. } = error.fault() else {
            panic!("expected BlobTooLarge, got {:?}", error.fault());
        };
        assert_eq!(*size, 19);
    }

    #[test]
    fn test_unborn_repository_refuses_head_as_unknown() {
        let directory = tempfile::tempdir().expect("temp dir");
        init(directory.path());
        let repository = Repository::open(directory.path()).expect("repository");
        let error = repository.resolve("HEAD").expect_err("unborn branch");
        assert!(matches!(
            error.fault(),
            HistoryFault::RevisionUnknown { .. }
        ));
    }

    #[test]
    fn test_every_fault_renders_its_registry_identity_and_evidence() {
        let cases: Vec<(HistoryFault, &str, &str)> = vec![
            (
                HistoryFault::Unversioned {
                    root: PathBuf::from("/workspace"),
                },
                "capability_unavailable",
                "workspace",
            ),
            (
                HistoryFault::RevisionUnknown {
                    rev: "feature/absent".to_owned(),
                },
                "resource_not_found",
                "rev",
            ),
            (
                HistoryFault::RevisionNotCommit {
                    rev: "main^{tree}".to_owned(),
                    kind: "tree".to_owned(),
                },
                "resource_not_found",
                "resolved_kind",
            ),
            (
                HistoryFault::TreeTooLarge { entries_max: 4 },
                "limit_exceeded",
                "entries_max",
            ),
            (
                HistoryFault::BlobTooLarge {
                    path: "src/lib.rs".to_owned(),
                    bytes_max: 4,
                    size: 19,
                },
                "limit_exceeded",
                "bytes_max",
            ),
            (
                HistoryFault::PathUnrepresentable {
                    path: "src/evil".to_owned(),
                },
                "unsupported_path",
                "path",
            ),
            (
                HistoryFault::Storage {
                    operation: "read blob",
                    detail: "object store gone".to_owned(),
                },
                "storage_failure",
                "operation",
            ),
        ];
        for (fault, code, evidence_key) in cases {
            let error = HistoryError::from(fault);
            assert_eq!(error.descriptor().code(), code, "{error}");
            assert!(
                error
                    .context()
                    .iter()
                    .any(|entry| entry.key() == evidence_key),
                "the {code} fault must carry {evidence_key} evidence: {error}"
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
            .tree_files(&head, &admit_all, 1)
            .expect_err("a subtree past the budget must refuse, not descend");
        assert!(matches!(error.fault(), HistoryFault::TreeTooLarge { .. }));
    }

    #[test]
    fn test_tree_files_refuses_a_non_utf8_committed_path() {
        let directory = repository_fixture();
        crate::fixture::commit_raw_path(directory.path(), b"evil-\xff.rs", "refs/heads/raw");
        let repository = Repository::open(directory.path()).expect("repository");
        let raw = repository.resolve("raw").expect("raw branch resolves");
        let error = repository
            .tree_files(&raw, &admit_all, REVISION_TREE_ENTRIES_MAX)
            .expect_err("a committed non-UTF-8 path must refuse");
        assert!(matches!(
            error.fault(),
            HistoryFault::PathUnrepresentable { .. }
        ));
        assert_eq!(error.descriptor().code(), "unsupported_path");
    }
}
