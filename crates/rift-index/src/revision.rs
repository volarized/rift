//! Builds a read index over one committed revision of the workspace.
//!
//! The composition mirrors the workspace scan with the history source
//! resolver in place of the directory walk: `git-history-source` supplies
//! committed file bytes, and the same syntax and index steps derive facts
//! from them, so a revision index answers every query the workspace index
//! answers. A future language joins revision reads the same way it joins
//! the scan - by its provider's declared extensions and a syntax step.

use std::path::PathBuf;

use rift_core::constants::WORKSPACE_IGNORED_DIRECTORIES;
use rift_core::{CompositionId, ProjectPath, SourceVisibility};
use rift_history::{REVISION_TREE_ENTRIES_MAX, Repository, ResolvedRevision};
use rift_provider::CompositionBuilder;
use rift_provider::ProviderComposition;

use crate::glob::PathMatcher;
use crate::workspace::{
    ReadIndex, RustFacts, WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexLimits,
    WorkspaceIndexViolation, component, composition_error, index_error_at, index_error_caused_by,
};

#[derive(Debug)]
pub(crate) struct RevisionFiles;

impl WorkspaceIndex {
    /// Builds read index over one committed tree.
    ///
    /// Hard floor and source policy apply to every path. Registered providers add
    /// syntax facts; every accepted UTF-8 file also enters baseline content catalog.
    ///
    /// # Errors
    ///
    /// Returns `WorkspaceIndexError` for invalid paths, bounds, history reads, or syntax.
    pub fn at_revision(
        repository: &Repository,
        revision: &ResolvedRevision,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
    ) -> Result<Self, WorkspaceIndexError> {
        Self::at_revision_with_languages(
            repository,
            revision,
            limits,
            visibility,
            &rift_core::TextFileInclusion::default(),
            &rift_core::LanguageFileSelections::default(),
        )
    }

    /// Builds one committed-tree index with configured language entries.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] for invalid paths, configuration,
    /// bounds, history reads, or syntax.
    pub fn at_revision_with_languages(
        repository: &Repository,
        revision: &ResolvedRevision,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &rift_core::TextFileInclusion,
        languages: &rift_core::LanguageFileSelections,
    ) -> Result<Self, WorkspaceIndexError> {
        let root = repository.root().to_path_buf();
        let composition = revision_composition()?;
        let language = std::sync::Arc::new(crate::WorkspaceLanguagePolicy::build(
            &root,
            languages,
            text_inclusion,
        )?);
        let matcher = PathMatcher::build(&root, visibility.include(), visibility.exclude())?;
        let includes = |path: &str| hard_floor_includes(path) && matcher.includes(&root.join(path));
        let listed = repository
            .tree_files(revision, &includes, REVISION_TREE_ENTRIES_MAX)
            .map_err(history_error)?;
        let mut catalog_bytes = 0_usize;
        let mut files = Vec::with_capacity(listed.len().min(limits.files_max()));
        let mut text_files = Vec::with_capacity(listed.len().min(limits.files_max()));
        for tree_file in &listed {
            let context_path = PathBuf::from(tree_file.path());
            if directory_depth(tree_file.path()) > limits.directory_depth_max() {
                return Err(index_error_at(
                    WorkspaceIndexViolation::TooDeep,
                    &context_path,
                ));
            }
            let bytes = match repository.blob_bytes(tree_file, limits.file_bytes_max()) {
                Ok(bytes) => bytes,
                Err(error)
                    if matches!(
                        error.fault(),
                        rift_history::HistoryFault::BlobTooLarge { .. }
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(history_error(error)),
            };
            if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
                continue;
            }
            let Some(class) = language.classifies(&context_path)? else {
                continue;
            };
            if text_files.len() >= limits.files_max() {
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
            let text_file = super::workspace::included_text_file(
                project_path,
                bytes,
                &context_path,
                limits,
                &mut catalog_bytes,
            )?;
            if let crate::language::ClassifiedPath::Source(provider) = class {
                files.push(super::workspace::indexed_file_from_catalog(
                    &text_file,
                    &context_path,
                    provider,
                )?);
            }
            text_files.push(text_file);
        }
        Self::from_parts(
            root,
            files,
            text_files,
            composition,
            limits,
            language,
            text_inclusion.clone(),
        )
    }
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
    use std::path::Path;

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

    /// The effective language table compiles before any blob is read, so an
    /// invalid `[search.text].include` pattern refuses the whole revision read.
    #[test]
    fn test_at_revision_refuses_an_invalid_text_include_pattern() {
        let directory = committed_workspace();
        let (repository, head) = open_head(directory.path());
        let error = WorkspaceIndex::at_revision_with_languages(
            &repository,
            &head,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["[".to_owned()], 1_024),
            &rift_core::LanguageFileSelections::default(),
        )
        .expect_err("an unclosed character class must refuse");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::SourcePatternInvalid
        );
    }

    /// An empty `[search.text].include` selects no plain text, so a committed
    /// path no language claims joins neither lane of the revision index.
    #[test]
    fn test_at_revision_with_an_empty_text_selection_drops_a_path_no_language_claims() {
        let directory = committed_workspace();
        let (repository, head) = open_head(directory.path());
        let index = WorkspaceIndex::at_revision_with_languages(
            &repository,
            &head,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(Vec::new(), 1_024),
            &rift_core::LanguageFileSelections::default(),
        )
        .expect("revision index");
        assert!(
            index
                .file(&ProjectPath::new("src/lib.rs").expect("path"))
                .is_some(),
            "a shipped language still claims its own committed path"
        );
        assert!(
            index
                .text_file(&ProjectPath::new("README.txt").expect("path"))
                .is_none(),
            "no text pattern selects the committed prose file"
        );
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
    fn test_at_revision_skips_oversized_blob_without_hiding_valid_text() {
        let directory = committed_workspace();
        let tight = WorkspaceIndexLimits::new(5, 8, 2_000, 4, 5).expect("limits");
        let index = revision_index(directory.path(), tight, &SourceVisibility::default())
            .expect("oversized blob must not hide valid revision text");
        assert!(
            index
                .text_file(&ProjectPath::new("README.txt").expect("path"))
                .is_some()
        );
        assert!(
            index
                .file(&ProjectPath::new("src/lib.rs").expect("path"))
                .is_none()
        );
        assert!(
            index
                .text_file(&ProjectPath::new("src/lib.rs").expect("path"))
                .is_none()
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
    fn test_at_revision_skips_non_utf8_blob_without_hiding_valid_source() {
        let directory = committed_workspace();
        fs::write(directory.path().join("evil.rs"), [0xff, 0xfe]).expect("binary blob");
        commit_all(directory.path(), "commit a binary rust path");
        let index = revision_index(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("invalid blob must not hide valid revision source");
        assert!(
            index
                .file(&ProjectPath::new("src/lib.rs").expect("path"))
                .is_some()
        );
        assert!(
            index
                .file(&ProjectPath::new("evil.rs").expect("path"))
                .is_none()
        );
        assert!(
            index
                .text_file(&ProjectPath::new("evil.rs").expect("path"))
                .is_none()
        );
    }
}
