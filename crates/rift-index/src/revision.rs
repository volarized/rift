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
use rift_error::{RiftError, errors};
use rift_history::{Repository, ResolvedRevision, TreeFile};
use rift_protocol::source::SOURCE_FILES_FIELD;
use rift_provider::CompositionBuilder;
use rift_provider::ProviderComposition;

use crate::language::ClassifiedPath;
use crate::workspace::{
    IndexContents, ReadIndex, RustFacts, WorkspaceIndex, WorkspaceIndexLimits, component,
};
use rift_analysis::PathMatcher;

#[derive(Debug)]
pub(crate) struct RevisionFiles;

/// Reads one committed file's bytes under the per-file byte bound it is handed; `None`
/// leaves the file out.
type CommittedBytes<'read> =
    dyn FnMut(&TreeFile, usize) -> Result<Option<Vec<u8>>, RiftError> + 'read;

impl WorkspaceIndex {
    /// Builds read index over one committed tree.
    ///
    /// Hard floor and source policy apply to every path. Registered providers add
    /// syntax facts; every accepted UTF-8 file also enters baseline content catalog.
    ///
    /// # Errors
    ///
    /// Returns `RiftError` for invalid paths, bounds, history reads, or syntax.
    pub fn at_revision(
        repository: &Repository,
        revision: &ResolvedRevision,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
    ) -> Result<Self, RiftError> {
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
    /// A committed file the index leaves out is absent from the index, as it is under
    /// the workspace scan: a blob past the per-file byte bound, bytes that are not UTF-8,
    /// or a syntax tree the provider refuses under one of its bounds. The index's
    /// warnings name each refused syntax tree.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid paths, configuration,
    /// bounds, history reads, or syntax.
    pub fn at_revision_with_languages(
        repository: &Repository,
        revision: &ResolvedRevision,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &rift_core::TextFileInclusion,
        languages: &rift_core::LanguageFileSelections,
    ) -> Result<Self, RiftError> {
        Self::at_revision_with_selection(
            repository,
            revision,
            limits,
            visibility,
            text_inclusion,
            languages,
            &|_| true,
        )
    }

    /// Builds one committed-tree index over the paths `selection` keeps.
    ///
    /// The visible-path policy applies first, so a selection can only narrow
    /// what the revision read would otherwise hold. A caller that already
    /// knows which paths it will read - a comparison of two revisions, which
    /// reads the changed ones alone - passes them here instead of paying for
    /// the whole tree.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid paths, configuration,
    /// bounds, history reads, or syntax.
    pub fn at_revision_with_selection(
        repository: &Repository,
        revision: &ResolvedRevision,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &rift_core::TextFileInclusion,
        languages: &rift_core::LanguageFileSelections,
        selection: &dyn Fn(&str) -> bool,
    ) -> Result<Self, RiftError> {
        Self::at_revision_with_blob_reader(
            repository,
            revision,
            limits,
            visibility,
            (text_inclusion, languages),
            selection,
            &mut |file, bytes_max| repository.blob_bytes(file, bytes_max).map(Some),
        )
    }

    /// Builds one committed-tree index over the paths `selection` keeps, reading
    /// each file's bytes through `read`, which is handed the per-file byte bound.
    ///
    /// `read` answering `None` leaves the file out with no warning; a blob past
    /// the byte bound is left out the way [`Self::at_revision_with_selection`]
    /// leaves it out. A comparison against the working tree reads the base
    /// side's files in the working form this way.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid paths, configuration,
    /// bounds, history reads, or syntax.
    pub fn at_revision_with_blob_reader(
        repository: &Repository,
        revision: &ResolvedRevision,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        (text_inclusion, languages): (
            &rift_core::TextFileInclusion,
            &rift_core::LanguageFileSelections,
        ),
        selection: &dyn Fn(&str) -> bool,
        read: &mut CommittedBytes<'_>,
    ) -> Result<Self, RiftError> {
        Self::at_revision_with_owner(
            (repository, rift_protocol::identity::SymbolOwner::Local),
            revision,
            limits,
            visibility,
            (text_inclusion, languages),
            selection,
            read,
        )
    }

    /// Builds a committed-tree index under one fixed local owner.
    ///
    /// # Errors
    /// Returns [`RiftError`] for invalid owner, paths, configuration, bounds, or history reads.
    pub fn at_revision_with_owner(
        (repository, owner): (&Repository, rift_protocol::identity::SymbolOwner),
        revision: &ResolvedRevision,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        (text_inclusion, languages): (
            &rift_core::TextFileInclusion,
            &rift_core::LanguageFileSelections,
        ),
        selection: &dyn Fn(&str) -> bool,
        read: &mut CommittedBytes<'_>,
    ) -> Result<Self, RiftError> {
        super::workspace::validate_project_owner(&owner)?;
        let root = repository.root().to_path_buf();
        let composition = revision_composition()?;
        let language = std::sync::Arc::new(crate::WorkspaceLanguagePolicy::build(
            &root,
            languages,
            text_inclusion,
        )?);
        let visible = RevisionPaths::build(&root, visibility)?;
        let includes = |path: &str| visible.includes(path) && selection(path);
        let listed =
            repository.tree_files(revision, &includes, limits.revision_tree_entries_max())?;
        let mut catalog_bytes = 0_usize;
        let mut contents = IndexContents::default();
        for tree_file in &listed {
            let context_path = PathBuf::from(tree_file.path());
            if directory_depth(tree_file.path()) > limits.directory_depth_max() {
                return errors::index::workspace_too_deep()
                    .path(&context_path)
                    .field("source.directory_depth")
                    .observed(directory_depth(tree_file.path()))
                    .maximum(limits.directory_depth_max())
                    .fail();
            }
            let bytes = match read(tree_file, limits.file_bytes_max()) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(error) if error.slug() == rift_error::errors::history::blob_too_large::SLUG => {
                    continue;
                }
                Err(error) => return error.fail(),
            };
            if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
                continue;
            }
            let Some(class) = language.classifies(&context_path)? else {
                continue;
            };
            if language.excludes_lockfile(&context_path) {
                continue;
            }
            if contents.held_count() >= limits.files_max() {
                return errors::index::workspace_too_many_files()
                    .path(&context_path)
                    .field(SOURCE_FILES_FIELD)
                    .observed(contents.held_count().saturating_add(1))
                    .maximum(limits.files_max())
                    .fail();
            }
            let project_path = ProjectPath::new(tree_file.path().to_owned()).map_err(|error| {
                errors::index::workspace_invalid_path()
                    .path(&context_path)
                    .cause(error)
                    .error()
            })?;
            let text_file = super::workspace::included_text_file(
                project_path,
                bytes,
                &context_path,
                limits,
                &mut catalog_bytes,
            )?;
            match class {
                ClassifiedPath::Source(provider) => {
                    contents.hold_source_file(
                        text_file,
                        &context_path,
                        provider,
                        limits.syntax(),
                    )?;
                }
                ClassifiedPath::Text => contents.hold_text_file(text_file),
            }
        }
        Self::from_parts_with_owner(
            (root, owner),
            contents,
            composition,
            limits,
            language,
            text_inclusion.clone(),
            super::WorkspaceContentCache::default(),
            None,
        )
    }
}

/// The committed paths one revision read may hold: the hard floor every
/// workspace applies, then the workspace's `[source]` policy.
///
/// One predicate owns both rules, so a caller that lists changed paths
/// before the index is built screens them the way the build itself would.
#[derive(Debug)]
pub struct RevisionPaths {
    root: PathBuf,
    matcher: PathMatcher,
}

impl RevisionPaths {
    /// Compiles the visible-path policy for the revision reads of one
    /// workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an invalid `[source]` pattern.
    pub fn build(root: &Path, visibility: &SourceVisibility) -> Result<Self, RiftError> {
        Ok(Self {
            root: root.to_path_buf(),
            matcher: PathMatcher::build_with_force_include(
                root,
                visibility.include(),
                visibility.exclude(),
                visibility.force_include(),
            )?,
        })
    }

    /// Whether one committed workspace-relative path is visible: its first
    /// segment stays outside the hard floor - `.git`, `.rift`, and `target`
    /// are never indexed - and the `[source]` policy keeps it.
    #[must_use]
    pub fn includes(&self, path: &str) -> bool {
        let first_segment = path.split('/').next().unwrap_or(path);
        !WORKSPACE_IGNORED_DIRECTORIES.contains(&first_segment)
            && self.matcher.includes(&self.root.join(path))
    }
}

/// The number of directories above a workspace-relative file path - the
/// depth the directory walk would have descended to reach it.
fn directory_depth(path: &str) -> usize {
    path.matches('/').count()
}

/// The revision read recipe: the history source resolver supplies committed
/// bytes to the same syntax and index steps the workspace scan uses.
fn revision_composition() -> Result<ProviderComposition, RiftError> {
    let source = component::<(), RevisionFiles>("git-history-source")?;
    let syntax = component::<RevisionFiles, RustFacts>("rust-tree-sitter")?;
    let index = component::<RustFacts, ReadIndex>("memory-index")?;
    let mut builder = CompositionBuilder::new(
        CompositionId::new("rust-revision-read")
            .map_err(|source| errors::index::workspace_composition().cause(source).error())?,
    );
    let files = builder.source("history", &source);
    let facts = builder.then(files, "syntax", &syntax);
    let reads = builder.then(facts, "index", &index);
    builder
        .output(reads)
        .build()
        .map_err(|source| errors::index::workspace_composition().cause(source).error())
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

    /// One predicate owns visibility, so a revision read keeps the paths
    /// `force_include` names exactly as the current-tree walk does.
    #[test]
    fn test_revision_paths_keep_a_force_included_path_include_never_names() {
        let directory = committed_workspace();
        let visibility = SourceVisibility::new(vec!["src/**".to_owned()], Vec::new(), true)
            .with_force_include(vec!["README.txt".to_owned()]);
        let visible = RevisionPaths::build(directory.path(), &visibility).expect("visible paths");

        assert!(visible.includes("src/lib.rs"));
        assert!(
            visible.includes("README.txt"),
            "a force_include match needs no include match at a revision either"
        );
        assert!(!visible.includes("other.txt"));
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
    ) -> Result<WorkspaceIndex, RiftError> {
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
            !index
                .nodes(&path, 4)
                .expect("node parse")
                .expect("indexed path")
                .is_empty(),
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
        assert_eq!(error.slug(), errors::analysis::source_pattern_invalid::SLUG);
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
            count_error.slug(),
            errors::index::workspace_too_many_files::SLUG
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
        assert_eq!(depth_error.slug(), errors::index::workspace_too_deep::SLUG);
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
        assert_eq!(error.slug(), errors::index::workspace_invalid_path::SLUG);
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

    /// A committed file the syntax provider refuses under its depth bound is left out the
    /// way the workspace scan leaves it out: the revision index still serves the file
    /// beside it, and its warnings name the refused one.
    #[test]
    fn test_at_revision_leaves_out_a_file_past_a_syntax_bound_and_serves_the_rest() {
        let directory = committed_workspace();
        let deep = format!(
            "pub fn deep() -> i32 {{ {open}1{close} }}\n",
            open = "(".repeat(600),
            close = ")".repeat(600),
        );
        fs::write(directory.path().join("src/deep.rs"), deep).expect("deep source");
        commit_all(
            directory.path(),
            "commit a source past the syntax depth bound",
        );
        let index = revision_index(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("a refused syntax tree must not fail the revision build");
        assert!(
            has_symbol(&index, "committed"),
            "the file beside it answers"
        );
        assert!(
            !has_symbol(&index, "deep"),
            "the refused file answers nothing"
        );
        let deep_path = ProjectPath::new("src/deep.rs").expect("path");
        assert!(index.file(&deep_path).is_none());
        assert!(index.text_file(&deep_path).is_none());
        assert_eq!(index.left_out_file_count(), 1);
        assert!(matches!(
            index.warnings(),
            [crate::WorkspaceIndexWarning::SyntaxTooLarge { path, .. }] if *path == deep_path
        ));
    }
}
