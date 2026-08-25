//! Builds a read index over one committed revision of the workspace.
//!
//! The composition mirrors the workspace scan with the history source
//! resolver in place of the directory walk: `git-history-source` supplies
//! committed file bytes, and the same syntax and index steps derive facts
//! from them, so a revision index answers every query the workspace index
//! answers. A future language joins revision reads the same way it joins
//! the scan - by its provider's declared extensions and a syntax step.

use std::path::{Path, PathBuf};

use rift_core::constants::WORKSPACE_IGNORED_DIRECTORIES;
use rift_core::{CompositionId, ProjectPath, SourceVisibility};
use rift_history::{REVISION_TREE_ENTRIES_MAX, Repository, ResolvedRevision, TreeFile};
use rift_provider::CompositionBuilder;
use rift_provider::ProviderComposition;

use crate::glob::PathMatcher;
use crate::workspace::{
    ReadIndex, RustFacts, WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexLimits,
    WorkspaceIndexViolation, component, composition_error, has_source_extension, included_file,
    index_error_at, index_error_caused_by, syntax_provider_for,
};

#[derive(Debug)]
pub(crate) struct RevisionFiles;

impl WorkspaceIndex {
    /// Builds a read index over `revision`'s committed files, applying the
    /// same `[source]` include/exclude policy, provider-declared extension
    /// inclusion, hard floor, and bounds as the workspace scan. The
    /// `.gitignore` chain is not consulted: every listed file is tracked,
    /// which is what ignoring would have prevented.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] for an invalid `[source]` pattern, a
    /// tree or file over a bound, an unreadable object store, or a committed
    /// path or file the index cannot represent.
    pub fn at_revision(
        repository: &Repository,
        revision: &ResolvedRevision,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
    ) -> Result<Self, WorkspaceIndexError> {
        let root = repository.root().to_path_buf();
        let composition = revision_composition()?;
        let matcher = PathMatcher::build(&root, visibility.include(), visibility.exclude())?;
        let includes = |path: &str| {
            has_source_extension(Path::new(path))
                && hard_floor_includes(path)
                && matcher.includes(&root.join(path))
        };
        let listed = repository
            .tree_files(revision, &includes, REVISION_TREE_ENTRIES_MAX)
            .map_err(history_error)?;
        let mut workspace_bytes = 0_usize;
        let mut files = Vec::with_capacity(listed.len().min(limits.files_max()));
        for tree_file in &listed {
            let context_path = PathBuf::from(tree_file.path());
            if directory_depth(tree_file.path()) > limits.directory_depth_max() {
                return Err(index_error_at(
                    WorkspaceIndexViolation::TooDeep,
                    &context_path,
                ));
            }
            if files.len() >= limits.files_max() {
                return Err(index_error_at(
                    WorkspaceIndexViolation::TooManyFiles,
                    &context_path,
                ));
            }
            let project_path = ProjectPath::new(tree_file.path().to_owned()).map_err(|error| {
                index_error_caused_by(
                    WorkspaceIndexViolation::InvalidPath,
                    Some(&context_path),
                    error,
                )
            })?;
            let bytes = blob_bytes(repository, tree_file, limits)?;
            files.push(included_file(
                project_path,
                bytes,
                &context_path,
                syntax_provider_for(&context_path),
                limits,
                &mut workspace_bytes,
            )?);
        }
        // Revision reads serve committed source only: text files join the workspace scan,
        // not a revision tree, so this list stays empty and the chunk bound is unused.
        Ok(Self::from_parts(
            root,
            files,
            Vec::new(),
            composition,
            limits,
            rift_core::TextFileInclusion::default(),
        ))
    }
}

/// One committed file's bytes, with the per-file byte bound enforced before
/// the blob is loaded.
fn blob_bytes(
    repository: &Repository,
    file: &TreeFile,
    limits: WorkspaceIndexLimits,
) -> Result<Vec<u8>, WorkspaceIndexError> {
    repository
        .blob_bytes(file, limits.file_bytes_max())
        .map_err(history_error)
}

/// Wraps one version-control failure, which keeps its own registry identity
/// and evidence through the `History` violation's delegation.
fn history_error(error: rift_history::HistoryError) -> WorkspaceIndexError {
    index_error_caused_by(WorkspaceIndexViolation::History, None, error)
}

/// Whether a committed path's first segment stays outside the hard floor
/// every workspace applies: `.git`, `.rift`, and `target` are never indexed.
fn hard_floor_includes(path: &str) -> bool {
    let first_segment = path.split('/').next().unwrap_or(path);
    !WORKSPACE_IGNORED_DIRECTORIES.contains(&first_segment)
}

/// The number of directories above a workspace-relative file path - the
/// depth the directory walk would have descended to reach it.
fn directory_depth(path: &str) -> usize {
    path.matches('/').count()
}

/// The revision read recipe: the history source resolver supplies committed
/// bytes to the same syntax and index steps the workspace scan uses.
fn revision_composition() -> Result<ProviderComposition, WorkspaceIndexError> {
    let source = component::<(), RevisionFiles>("git-history-source")?;
    let syntax = component::<RevisionFiles, RustFacts>("rust-tree-sitter")?;
    let index = component::<RustFacts, ReadIndex>("memory-index")?;
    let mut builder = CompositionBuilder::new(
        CompositionId::new("rust-revision-read").map_err(composition_error)?,
    );
    let files = builder.source("history", &source);
    let facts = builder.then(files, "syntax", &syntax);
    let reads = builder.then(facts, "index", &index);
    builder.output(reads).build().map_err(composition_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_history::fixture::{commit_all, init};
    use std::fs;

    fn committed_workspace() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temp dir");
        init(directory.path());
        fs::create_dir_all(directory.path().join("src")).expect("directory");
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn committed() {}\n",
        )
        .expect("source");
        fs::write(directory.path().join("README.txt"), "prose\n").expect("prose");
        commit_all(directory.path(), "introduce committed");
        directory
    }

    fn open_head(root: &Path) -> (Repository, ResolvedRevision) {
        let repository = Repository::open(root).expect("repository");
        let head = repository.resolve("main").expect("head resolves");
        (repository, head)
    }

    fn revision_index(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
    ) -> Result<WorkspaceIndex, WorkspaceIndexError> {
        let (repository, head) = open_head(root);
        WorkspaceIndex::at_revision(&repository, &head, limits, visibility)
    }

    fn has_symbol(index: &WorkspaceIndex, name: &str) -> bool {
        !index.symbols(name, 5).expect("symbol read").is_empty()
    }

    #[test]
    fn test_at_revision_serves_the_committed_tree_not_the_working_tree() {
        let directory = committed_workspace();
        fs::write(directory.path().join("src/lib.rs"), "pub fn drifted() {}\n")
            .expect("working-tree drift");
        let index = revision_index(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("revision index");
        assert!(
            has_symbol(&index, "committed"),
            "the committed declaration answers"
        );
        assert!(
            !has_symbol(&index, "drifted"),
            "working-tree drift is invisible"
        );
        let paths: Vec<&str> = index.files().map(|file| file.path().as_str()).collect();
        assert_eq!(paths, ["src/lib.rs"], "prose files stay outside the index");
        let path = ProjectPath::new("src/lib.rs").expect("path");
        assert!(
            !index.nodes(&path, 4).expect("indexed path").is_empty(),
            "syntax nodes parse from committed bytes"
        );
        let lines = index.source_matches("committed", 5).expect("lexical read");
        assert_eq!(lines[0].1, 1);
    }

    /// A committed `.md` file joins revision reads through the markdown
    /// provider's declared extension, like any other source file.
    #[test]
    fn test_at_revision_serves_committed_markdown_headings() {
        let directory = committed_workspace();
        fs::write(
            directory.path().join("docs.md"),
            "# Install\n\nRun the beacon.\n",
        )
        .expect("markdown fixture");
        commit_all(directory.path(), "introduce docs");
        let index = revision_index(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("revision index");
        let matches = index.symbols("Install", 5).expect("symbol read");
        assert_eq!(matches[0].symbol.qualified_name, "Install");
        assert_eq!(matches[0].symbol.kind, "heading");
        assert_eq!(matches[0].file.syntax().language().name, "markdown");
    }

    /// Committed `.json` and `.yml` files join revision reads through their
    /// claiming providers' declared extensions, like any other source file.
    #[test]
    fn test_at_revision_serves_committed_json_and_yaml_members() {
        let directory = committed_workspace();
        fs::write(
            directory.path().join("config.json"),
            "{\"server\": {\"port\": 8080}}\n",
        )
        .expect("json fixture");
        fs::write(directory.path().join("deploy.yml"), "retries: 3\n").expect("yaml fixture");
        commit_all(directory.path(), "introduce configuration");
        let index = revision_index(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("revision index");
        let members = index.symbols("port", 5).expect("symbol read");
        assert_eq!(members[0].symbol.qualified_name, "server > port");
        assert_eq!(members[0].symbol.kind, "member");
        assert_eq!(members[0].file.syntax().language().name, "json");
        let entries = index.symbols("retries", 5).expect("symbol read");
        assert_eq!(entries[0].symbol.qualified_name, "retries");
        assert_eq!(entries[0].symbol.kind, "mapping_entry");
        assert_eq!(entries[0].file.syntax().language().name, "yaml");
    }

    #[test]
    fn test_at_revision_applies_source_policy_and_hard_floor() {
        let directory = committed_workspace();
        fs::create_dir_all(directory.path().join("vendor")).expect("directory");
        fs::write(
            directory.path().join("vendor/dep.rs"),
            "pub fn vendored() {}\n",
        )
        .expect("source");
        fs::create_dir_all(directory.path().join("target")).expect("directory");
        fs::write(
            directory.path().join("target/gen.rs"),
            "pub fn floor() {}\n",
        )
        .expect("source");
        commit_all(directory.path(), "commit vendored and floor files");
        let visibility = SourceVisibility::new(Vec::new(), vec!["vendor/**".to_owned()], true);
        let index = revision_index(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
        )
        .expect("revision index");
        assert!(has_symbol(&index, "committed"));
        assert!(!has_symbol(&index, "vendored"), "[source] exclude applies");
        assert!(!has_symbol(&index, "floor"), "the hard floor applies");
    }

    #[test]
    fn test_at_revision_composition_names_the_history_source_step() {
        let directory = committed_workspace();
        let index = revision_index(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("revision index");
        let steps: Vec<&str> = index
            .composition()
            .steps()
            .iter()
            .map(|step| step.component().as_str())
            .collect();
        assert_eq!(
            steps,
            ["git-history-source", "rust-tree-sitter", "memory-index"]
        );
    }

    #[test]
    fn test_at_revision_refuses_file_count_and_depth_bounds() {
        let directory = committed_workspace();
        fs::write(directory.path().join("extra.rs"), "pub fn extra() {}\n").expect("source");
        commit_all(directory.path(), "second source file");
        let one_file = WorkspaceIndexLimits::new(1, 1_000, 2_000, 4, 5).expect("limits");
        let count_error = revision_index(directory.path(), one_file, &SourceVisibility::default())
            .expect_err("two committed sources must refuse a one-file bound");
        assert_eq!(
            count_error.fault().violation(),
            WorkspaceIndexViolation::TooManyFiles
        );

        fs::create_dir_all(directory.path().join("deep/nest")).expect("directories");
        fs::write(
            directory.path().join("deep/nest/lowest.rs"),
            "pub fn lowest() {}\n",
        )
        .expect("source");
        commit_all(directory.path(), "deeply nested source");
        let shallow = WorkspaceIndexLimits::new(5, 1_000, 4_000, 1, 5).expect("limits");
        let depth_error = revision_index(directory.path(), shallow, &SourceVisibility::default())
            .expect_err("deep/nest/lowest.rs must refuse a one-level depth bound");
        assert_eq!(
            depth_error.fault().violation(),
            WorkspaceIndexViolation::TooDeep
        );
    }

    #[test]
    fn test_at_revision_refuses_an_oversized_committed_blob() {
        let directory = committed_workspace();
        let tight = WorkspaceIndexLimits::new(5, 8, 2_000, 4, 5).expect("limits");
        let error = revision_index(directory.path(), tight, &SourceVisibility::default())
            .expect_err("a committed source over the byte bound must refuse");
        assert_eq!(error.fault().violation(), WorkspaceIndexViolation::History);
        assert_eq!(
            error.descriptor().code(),
            "limit_exceeded",
            "the blob bound keeps the version-control failure's classification"
        );
        let context = error.context();
        assert!(
            context
                .iter()
                .any(|entry| entry.key() == "path" && entry.value() == "src/lib.rs"),
            "the refusal names the oversized committed file: {context:?}"
        );
    }

    #[test]
    fn test_at_revision_refuses_a_committed_path_the_project_contract_forbids() {
        let directory = committed_workspace();
        // A backslash is legal in a git tree entry and on unix filesystems,
        // and `ProjectPath` refuses it on every platform; plumbing commits it
        // without touching the host filesystem.
        rift_history::fixture::commit_raw_path(directory.path(), b"bad\\path.rs", "refs/heads/raw");
        let (repository, _) = open_head(directory.path());
        let raw = repository.resolve("raw").expect("raw branch resolves");
        let error = WorkspaceIndex::at_revision(
            &repository,
            &raw,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect_err("a committed backslash path must refuse");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidPath
        );
    }

    #[test]
    fn test_at_revision_refuses_a_non_utf8_committed_blob() {
        let directory = committed_workspace();
        fs::write(directory.path().join("evil.rs"), [0xff, 0xfe]).expect("binary blob");
        commit_all(directory.path(), "commit a binary rust path");
        let error = revision_index(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect_err("a non-UTF-8 committed source must refuse");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidSource
        );
    }
}
