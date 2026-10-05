use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::Read as _;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, Match, Walk, WalkBuilder};
pub use rift_analysis::IndexedFile;
use rift_analysis::documentation::{DocumentationCollection, DocumentationLayer};
use rift_core::constants::{
    READ_RESULTS_MAX_DEFAULT, VCS_IGNORE_FILE, WORKSPACE_BYTES_MAX_DEFAULT,
    WORKSPACE_CONFIGURATION_FILE, WORKSPACE_DECLARATIONS_MAX_DEFAULT,
    WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT, WORKSPACE_FILES_MAX_DEFAULT,
    WORKSPACE_IGNORED_DIRECTORIES,
};
use rift_core::{
    CompositionId, LanguageFileSelections, PortableSymbolFacts, ProjectPath, ProviderId,
    SourceVisibility, SymbolId, TextFileInclusion, symbol_identity,
};
use rift_error::{ErrorSlug, RiftError, ctx, errors};
use rift_protocol::configuration::{LargeFileStrategy, SyntaxConfiguration};
use rift_protocol::documentation::{DocumentationContentIdentity, DocumentationSourceIdentity};
use rift_protocol::search::FORCE_INCLUDE_FIELD;
use rift_protocol::source::{
    SOURCE_DECLARATIONS_FIELD, SOURCE_FILES_FIELD, SOURCE_WORKSPACE_SIZE_FIELD,
};
use rift_provider::{
    AssembledSymbol, Component, CompositionBuilder, NormalizedGraph, ProviderComposition,
};
use rift_ranking::{
    DOCUMENTATION_BYTES_MAX, DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation,
    IDENTIFIER_TERMS_BYTES_MAX, IdentifierMatchClass, IdentifierRanking, IndexDocument,
    ParsedQuery, RankingInput, SIGNATURE_BYTES_MAX, SearchableField, identifier_terms, match_class,
};
use rift_syntax::{SyntaxLimits, SyntaxNode, SyntaxProvider, SyntaxSource, SyntaxSymbol, registry};
use sha2::{Digest as _, Sha256};

use crate::capture::{CaptureBoundary, CapturedPath, LastCapture, capture_path, root_identity};
use crate::change_set::{FileDigest, FileRecord, PathChanges, WorkspaceDigests, tree_revision_of};
use crate::chunk::text_chunks;
use crate::content_cache::WorkspaceContentCache;
use crate::documentation::NotebookFiles;
use crate::language::{ClassifiedPath, WorkspaceLanguagePolicy};
use crate::relationship::RelationshipStore;
use crate::semantic::{BuiltSemantics, WorkspaceSemantics};
use rift_analysis::{DocumentationSelection, ForceIncludeReach, PathMatcher, PathVerdict};

#[derive(Debug)]
pub(crate) struct WorkspaceFiles;
#[derive(Debug)]
pub(crate) struct RustFacts;
#[derive(Debug)]
pub(crate) struct ReadIndex;

/// Direct-workspace scan and result bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub struct WorkspaceIndexLimits {
    files_max: usize,
    file_bytes_max: usize,
    workspace_bytes_max: usize,
    declarations_max: usize,
    directory_depth_max: usize,
    results_max: usize,
    syntax: SyntaxLimits,
    large_files: LargeFileStrategy,
}

impl WorkspaceIndexLimits {
    /// Constructs positive direct-workspace bounds.
    ///
    /// The declaration bound is [`WORKSPACE_DECLARATIONS_MAX_DEFAULT`]; the `[source]`
    /// table replaces it through [`Self::with_workspace_bounds`], beside the two bounds
    /// it owns.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when any bound is zero.
    pub fn new(
        files_max: usize,
        file_bytes_max: usize,
        workspace_bytes_max: usize,
        directory_depth_max: usize,
        results_max: usize,
    ) -> Result<Self, RiftError> {
        Self {
            files_max,
            file_bytes_max,
            workspace_bytes_max,
            declarations_max: WORKSPACE_DECLARATIONS_MAX_DEFAULT,
            directory_depth_max,
            results_max,
            syntax: SyntaxLimits::default(),
            large_files: LargeFileStrategy::default(),
        }
        .validated()
    }

    /// Refuses any bound that is zero, so no build runs under a bound it cannot meet.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when any bound is zero.
    fn validated(self) -> Result<Self, RiftError> {
        for bound in self.bounds() {
            positive_bound(bound)?;
        }
        Ok(self)
    }

    const fn bounds(self) -> [usize; 6] {
        [
            self.files_max,
            self.file_bytes_max,
            self.workspace_bytes_max,
            self.declarations_max,
            self.directory_depth_max,
            self.results_max,
        ]
    }

    /// The same per-file, depth, and result bounds under the `[source]` table's own
    /// `files`, `workspace_size`, and `declarations`.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when any of the three bounds is zero.
    pub fn with_workspace_bounds(
        self,
        files_max: usize,
        workspace_bytes_max: usize,
        declarations_max: usize,
    ) -> Result<Self, RiftError> {
        Self {
            files_max,
            workspace_bytes_max,
            declarations_max,
            ..self
        }
        .validated()
    }

    /// Returns maximum result count accepted per query.
    #[must_use]
    pub const fn results_max(self) -> usize {
        self.results_max
    }

    /// Returns maximum source files accepted per index.
    #[must_use]
    pub const fn files_max(self) -> usize {
        self.files_max
    }

    /// Returns maximum declarations the publication this index builds holds together.
    #[must_use]
    pub const fn declarations_max(self) -> usize {
        self.declarations_max
    }

    /// Returns maximum bytes accepted for one source file.
    pub(crate) const fn file_bytes_max(self) -> usize {
        self.file_bytes_max
    }

    /// Returns maximum directory depth an index reaches below the root.
    pub(crate) const fn directory_depth_max(self) -> usize {
        self.directory_depth_max
    }

    /// Returns maximum aggregate source bytes accepted per index.
    pub(crate) const fn workspace_bytes_max(self) -> usize {
        self.workspace_bytes_max
    }

    /// Parses every source under `syntax`; the per-file byte bound follows its source bound
    /// under the large-file strategy these bounds carry, as [`Self::with_large_files`]
    /// states.
    #[must_use]
    pub const fn with_syntax(self, syntax: SyntaxLimits) -> Self {
        Self { syntax, ..self }.with_large_files(self.large_files)
    }

    /// Parses every source under a `[providers.syntax]` table's bounds, as
    /// [`Self::with_syntax`] does.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the table states a zero bound.
    pub fn with_syntax_configuration(
        self,
        configuration: &SyntaxConfiguration,
    ) -> Result<Self, RiftError> {
        let syntax = SyntaxLimits::from_configuration(configuration)
            .map_err(|error| errors::index::workspace_zero_limit().cause(error).error())?;
        Ok(self.with_syntax(syntax))
    }

    /// Syntax bounds every source parses under.
    #[must_use]
    pub const fn syntax(self) -> SyntaxLimits {
        self.syntax
    }

    /// The per-file byte bound `[search.text] large_files` sets: under `split`, the default,
    /// a file is held as text up to the workspace byte bound whatever its size, and one past
    /// the syntax source bound is held unparsed; under `skip` a file past the syntax source
    /// bound is left out, as the provider cannot parse it. [`Self::with_syntax`] keeps the
    /// strategy and derives the bound again from the new source bound.
    #[must_use]
    pub const fn with_large_files(self, strategy: LargeFileStrategy) -> Self {
        let parsed = self.syntax.source_bytes_max();
        let file_bytes_max = match strategy {
            LargeFileStrategy::Split if self.workspace_bytes_max > parsed => {
                self.workspace_bytes_max
            }
            LargeFileStrategy::Split | LargeFileStrategy::Skip => parsed,
        };
        Self {
            file_bytes_max,
            large_files: strategy,
            ..self
        }
    }
}

impl Default for WorkspaceIndexLimits {
    /// The served defaults: `[source]` and `[providers.syntax]` at their defaults, and the
    /// per-file byte bound the default `[search.text] large_files`, `split`, sets.
    fn default() -> Self {
        Self {
            files_max: WORKSPACE_FILES_MAX_DEFAULT,
            file_bytes_max: registry::file_bytes_max_default(),
            workspace_bytes_max: WORKSPACE_BYTES_MAX_DEFAULT,
            declarations_max: WORKSPACE_DECLARATIONS_MAX_DEFAULT,
            directory_depth_max: WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT,
            results_max: READ_RESULTS_MAX_DEFAULT,
            syntax: SyntaxLimits::default(),
            large_files: LargeFileStrategy::default(),
        }
        .with_large_files(LargeFileStrategy::default())
    }
}

/// The documentation selection the `[documentation]` table `inclusion` carries compiles
/// to, under the same `[source]` glob semantics every other index pattern matches with.
fn documentation_selection(
    inclusion: &TextFileInclusion,
) -> Result<DocumentationSelection, RiftError> {
    DocumentationSelection::new(inclusion.documentation())
}

/// One immutable visible UTF-8 file in the baseline content catalog.
///
/// A file over `[search.text].max_chunk` lands here whole. Search unit
/// derivation splits its content while keeping one file identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSourceFile {
    path: ProjectPath,
    content: Arc<String>,
    digest: FileDigest,
    executable: bool,
}

impl TextSourceFile {
    /// One catalog file from its path and UTF-8 content, digested over that content.
    pub(crate) fn from_content(path: ProjectPath, content: String) -> Self {
        Self {
            path,
            digest: FileDigest::of(content.as_bytes()),
            content: Arc::new(content),
            executable: false,
        }
    }

    /// Returns project-relative path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns complete UTF-8 content.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Returns the digest of the bytes this file was indexed from.
    #[must_use]
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }

    /// Whether this file was executable when the catalog captured it.
    #[must_use]
    pub const fn executable(&self) -> bool {
        self.executable
    }
}

/// Symbol plus source file matched by read index.
///
/// `Eq` is not derived: `symbol` and `file` carry types that are not `Eq`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SymbolMatch<'a> {
    /// Containing file.
    pub file: &'a IndexedFile,
    /// Matched declaration.
    pub symbol: &'a SyntaxSymbol,
    /// Stable semantic match priority.
    pub rank: IdentifierMatchClass,
}

/// Normalized symbol fields required by read results.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadableSymbol {
    assembled: AssembledSymbol,
    facts: PortableSymbolFacts,
}

impl ReadableSymbol {
    fn new(assembled: AssembledSymbol) -> Option<Self> {
        let facts = assembled.facts()?.clone();
        Some(Self { assembled, facts })
    }

    /// The readable symbol `semantics` assembles for one syntax-provider identity.
    pub(crate) fn assembled_by(semantics: &WorkspaceSemantics, identity: &str) -> Option<Self> {
        semantics.assembled(identity).and_then(Self::new)
    }

    /// Returns complete normalized assembly.
    #[must_use]
    pub const fn assembled(&self) -> &AssembledSymbol {
        &self.assembled
    }

    /// Returns established normalized symbol identity when available.
    #[must_use]
    pub const fn identity(&self) -> Option<&SymbolId> {
        self.assembled.identity()
    }

    /// Returns selected and combined portable facts.
    #[must_use]
    pub const fn facts(&self) -> &PortableSymbolFacts {
        &self.facts
    }

    /// Returns the read symbol carried by the normalized presentation.
    #[must_use]
    pub fn to_protocol_symbol(&self) -> rift_protocol::read::Symbol {
        self.assembled.to_protocol_symbol(&self.facts)
    }
}

#[derive(Debug)]
struct ReadableSymbolMissing {
    identity: String,
}

impl std::fmt::Display for ReadableSymbolMissing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "normalized Contribution graph has no readable symbol for {}",
            self.identity
        )
    }
}

impl std::error::Error for ReadableSymbolMissing {}

/// Exact identity of visible workspace source paths and bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFingerprint([u8; 32]);

/// Compiled source visibility used by filesystem event inclusion.
#[derive(Debug)]
pub struct WorkspaceSourcePolicy {
    root: PathBuf,
    watched_root: PathBuf,
    limits: WorkspaceIndexLimits,
    matcher: PathMatcher,
    gitignore: Option<GitignoreChain>,
    language: WorkspaceLanguagePolicy,
}

/// Separates one path from its content digest in workspace identity material.
const FINGERPRINT_PATH_SEPARATOR: u8 = 0;
/// Separates adjacent files in workspace identity material.
const FINGERPRINT_FILE_SEPARATOR: u8 = 0xff;

impl WorkspaceFingerprint {
    /// Captures visible file paths and bytes without parsing syntax.
    ///
    /// Work is bounded by [`WorkspaceIndexLimits`]. A claimed file whose bytes are not
    /// UTF-8 is omitted from the capture rather than failing it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for discovery, read, or configured-bound failures.
    pub fn capture(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
    ) -> Result<Self, RiftError> {
        Ok(capture_digests(root, limits, visibility)?.fingerprint())
    }

    /// Eight-hex-character rendering of this fold, for spans and diagnostics.
    ///
    /// The fold covers every recorded file, so this moves when a baseline text file or a
    /// left-out file moves and the syntax-indexed tree revision does not. A diagnostic
    /// that carries both tells those two apart.
    #[must_use]
    pub fn wire_revision(&self) -> String {
        let mut revision = String::with_capacity(8);
        for byte in &self.0[..4] {
            use std::fmt::Write as _;
            let _ = write!(revision, "{byte:02x}");
        }
        revision
    }

    /// Folds one publication's own files, absorbing each file's digest rather than its
    /// bytes. Files already carry the digest of what the index read, so this costs one
    /// hash update per file however large the workspace's sources are.
    fn from_files(
        files: &BTreeMap<ProjectPath, Arc<IndexedFile>>,
        text_files: &BTreeMap<ProjectPath, Arc<TextSourceFile>>,
        left_out: &BTreeMap<ProjectPath, LeftOutFileState>,
    ) -> Self {
        Self::from_digests(&keyed_digests(files, text_files, left_out))
    }

    /// Folds every visible file's digest in project-path order.
    ///
    /// Syntax and baseline text files fold as one ordered set, and the order is the
    /// project path's - never the order a directory walk produced, which sorts `docs/a.rs`
    /// and `docs-x/a.rs` the other way round, and never source-then-text, which interleaves
    /// differently again. Both constructions fold here, so an index and a request-time
    /// capture of one tree cannot disagree.
    fn from_digests(digests: &WorkspaceDigests) -> Self {
        let mut hasher = Sha256::new();
        for (path, digest) in digests.iter() {
            update_fingerprint(&mut hasher, path, digest);
        }
        Self(hasher.finalize().into())
    }

    /// Derives non-zero revision number for this captured publication.
    fn revision_number(&self) -> u64 {
        let mut prefix = [0_u8; size_of::<u64>()];
        prefix.copy_from_slice(&self.0[..size_of::<u64>()]);
        u64::from_be_bytes(prefix).max(1)
    }
}

impl WorkspaceSourcePolicy {
    /// Compiles path policy from accepted configuration and `.gitignore` files.
    ///
    /// Work is bounded by [`WorkspaceIndexLimits`].
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid roots, patterns, ignore files,
    /// or configured-bound failures.
    pub fn build(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
    ) -> Result<Self, RiftError> {
        Self::build_with_languages(
            root,
            limits,
            visibility,
            text_inclusion,
            &LanguageFileSelections::default(),
        )
    }

    /// Compiles path policy with configured language entries.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid roots, patterns, ignore files,
    /// or configured-bound failures.
    pub fn build_with_languages(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
    ) -> Result<Self, RiftError> {
        Self::build_with_languages_cancellable(
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
            &|| false,
        )
    }

    /// Compiles path policy, checking `cancelled` between ignore files.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid roots, patterns, ignore files,
    /// configured-bound failures, or cancellation.
    pub fn build_with_languages_cancellable(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let watched_root = root.to_path_buf();
        let root = canonical_root(root)?;
        let matcher = PathMatcher::build_with_force_include(
            &root,
            visibility.include(),
            visibility.exclude(),
            visibility.force_include(),
        )?;
        let language = WorkspaceLanguagePolicy::build(&root, languages, text_inclusion)?;
        let gitignore = visibility
            .respect_gitignore()
            .then(|| GitignoreChain::build_cancellable(&root, limits, cancelled))
            .transpose()?;
        Ok(Self {
            root,
            watched_root,
            limits,
            matcher,
            gitignore,
            language,
        })
    }

    /// Whether `path` is visible to the workspace: above the hard floor, kept by the
    /// `[source]` policy, and not excluded by the workspace's `.gitignore` chain.
    /// Visibility is what the change tools reach. Which lane an indexed path joins is a
    /// separate question the effective language table answers - see
    /// [`Self::language_for_path`].
    #[must_use]
    pub fn visible(&self, path: &Path) -> bool {
        let Some(path) = self.normalized_path(path) else {
            return false;
        };
        self.visible_normalized(path.as_ref())
    }

    /// Effective language entry matching one path after visibility accepts it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when two language entries match.
    pub fn language_for_path(
        &self,
        path: &Path,
    ) -> Result<Option<&crate::EffectiveLanguage>, RiftError> {
        let Some(path) = self.normalized_path(path) else {
            return Ok(None);
        };
        if !self.visible_normalized(path.as_ref()) {
            return Ok(None);
        }
        self.language.language_for_path(path.as_ref())
    }

    /// Effective language entries and text selection used by this policy.
    #[must_use]
    pub const fn language_policy(&self) -> &WorkspaceLanguagePolicy {
        &self.language
    }

    /// Returns every visible regular-file path in project-path order.
    ///
    /// Work is bounded by the configured file-count and directory-depth limits.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for discovery or configured-bound failures.
    pub fn visible_paths(&self) -> Result<Vec<ProjectPath>, RiftError> {
        self.visible_paths_cancellable(&|| false)
    }

    fn visible_paths_cancellable(
        &self,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<ProjectPath>, RiftError> {
        check_cancelled(cancelled)?;
        let mut paths = Vec::new();
        for entry in source_walk(
            &self.root,
            self.limits.directory_depth_max,
            GitignorePolicy::Ignore,
        ) {
            check_cancelled(cancelled)?;
            let entry = entry.map_err(|error| walk_error(&self.root, error))?;
            let file_type = entry.file_type();
            if file_type.is_some_and(|file_type| file_type.is_dir()) {
                if entry.depth() > self.limits.directory_depth_max {
                    return errors::index::workspace_too_deep()
                        .path(entry.path())
                        .field("source.directory_depth")
                        .observed(entry.depth() as u64)
                        .maximum(self.limits.directory_depth_max as u64)
                        .fail();
                }
                continue;
            }
            if !file_type.is_some_and(|file_type| file_type.is_file())
                || !self.visible_selection(entry.path())
            {
                continue;
            }
            if paths.len() >= self.limits.files_max {
                return errors::index::workspace_too_many_files()
                    .path(entry.path())
                    .field(SOURCE_FILES_FIELD)
                    .observed(paths.len().saturating_add(1))
                    .maximum(self.limits.files_max)
                    .fail();
            }
            let relative = entry.path().strip_prefix(&self.root).map_err(|error| {
                errors::index::workspace_invalid_path()
                    .path(entry.path())
                    .source(error)
                    .error()
            })?;
            paths.push(relative_path(relative)?);
        }
        paths.sort();
        Ok(paths)
    }

    /// Reads one visible path's digest within the configured file bound.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for reads or a file crossing the configured bound.
    pub fn visible_digest(&self, path: &Path) -> Result<Option<FileDigest>, RiftError> {
        if !self.visible(path) {
            return Ok(None);
        }
        let handle = match fs::File::open(path) {
            Ok(handle) => handle,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return errors::index::workspace_filesystem()
                    .path(path)
                    .source(error)
                    .fail();
            }
        };
        let ceiling = u64::try_from(self.limits.file_bytes_max)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();
        handle
            .take(ceiling)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                errors::index::workspace_filesystem()
                    .path(path)
                    .source(error)
                    .error()
            })?;
        if bytes.len() > self.limits.file_bytes_max {
            return errors::index::workspace_file_too_large()
                .path(path)
                .field("source.file_bytes")
                .observed(bytes.len() as u64)
                .maximum(self.limits.file_bytes_max as u64)
                .fail();
        }
        Ok(Some(FileDigest::of(&bytes)))
    }

    /// Captures every visible regular file's content digest. A file the index leaves out
    /// under the per-file byte bound is absent from the capture, as it is from the index.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for a read that fails or a walk past the file count
    /// bound.
    pub fn visible_digests(&self) -> Result<WorkspaceDigests, RiftError> {
        let mut digests = Vec::new();
        for path in self.visible_paths()? {
            let absolute = self.root.join(path.as_str());
            match self.visible_digest(&absolute) {
                Ok(Some(digest)) => digests.push((path, digest)),
                Ok(None) => {}
                Err(error) => match left_out_file(error, path.clone()) {
                    Ok(Some(_)) => {}
                    Ok(None) => unreachable!("recognized file refusal always names a warning"),
                    Err(error) => return error.fail(),
                },
            }
        }
        Ok(WorkspaceDigests::new(digests))
    }

    /// Carries the checks [`Self::visible`] and [`Self::includes`] share, against a path
    /// [`Self::normalized_path`] already resolved: above the hard floor, then the `[source]`
    /// globs in their own precedence, with the workspace's `.gitignore` chain deciding only
    /// what `force_include` did not already keep. Neither caller normalizes twice.
    fn visible_normalized(&self, path: &Path) -> bool {
        let above_hard_floor = hard_floor_includes_path(&self.root, path);
        if !above_hard_floor {
            return false;
        }
        self.visible_selection(path)
    }

    /// Applies source selection to a path whose traversal already checked the hard floor.
    fn visible_selection(&self, path: &Path) -> bool {
        match self.matcher.verdict(path) {
            PathVerdict::Excluded | PathVerdict::NotIncluded => false,
            PathVerdict::ForceIncluded => true,
            PathVerdict::Included => self
                .gitignore
                .as_ref()
                .is_none_or(|gitignore| !gitignore.excludes(path, false)),
        }
    }

    /// Returns whether one directory can contain visible Rust source.
    #[must_use]
    pub fn may_include_descendant(&self, path: &Path) -> bool {
        let Some(path) = self.normalized_path(path) else {
            return false;
        };
        let path = path.as_ref();
        let above_hard_floor = hard_floor_includes_path(&self.root, path);
        if !above_hard_floor {
            return false;
        }
        if !self.matcher.may_include_descendant(path) {
            return false;
        }
        if self.matcher.force_include_reaches(path) {
            return true;
        }
        self.gitignore
            .as_ref()
            .is_none_or(|gitignore| !gitignore.excludes(path, true))
    }

    /// Maps one event path onto the project-relative path the index keys files by, or
    /// nothing when the path lies outside this policy's root.
    ///
    /// A watcher reports absolute paths and a change tool reports project-relative ones;
    /// both reach the index through this one normalization, so an event and a rebuild
    /// cannot key the same file two ways.
    #[must_use]
    pub fn project_path(&self, path: &Path) -> Option<ProjectPath> {
        let normalized = self.normalized_path(path)?;
        let relative = normalized.strip_prefix(&self.root).ok()?;
        relative_path(relative).ok()
    }

    /// Whether writing this file changes what the workspace includes, so a rebuild after
    /// it covers every visible file rather than that file alone.
    ///
    /// The workspace's own `rift.toml` selects the `[source]` policy and every
    /// `.gitignore` below the root narrows it, so neither is a file the index can reparse
    /// on its own. A `.gitignore` under a directory this policy already excludes decides
    /// nothing, because no file below it is indexed either way.
    #[must_use]
    pub fn decides_inclusion(&self, path: &Path) -> bool {
        if self.is_workspace_configuration(path) {
            return true;
        }
        let Some(normalized) = self.normalized_path(path) else {
            return false;
        };
        let normalized = normalized.as_ref();
        normalized.file_name() == Some(OsStr::new(VCS_IGNORE_FILE))
            && normalized
                .parent()
                .is_some_and(|parent| self.may_include_descendant(parent))
    }

    /// Whether `path` names this workspace's own `rift.toml`.
    ///
    /// The watcher reports a path in whatever spelling the platform hands it, and a
    /// macOS temporary root reaches the watcher through a symlink, so the comparison
    /// runs on the normalized form rather than on the bytes the event carried.
    #[must_use]
    pub fn is_workspace_configuration(&self, path: &Path) -> bool {
        self.normalized_path(path).is_some_and(|normalized| {
            normalized.as_ref() == self.root.join(WORKSPACE_CONFIGURATION_FILE)
        })
    }

    /// Maps watched spelling onto canonical root without touching event path.
    fn normalized_path<'a>(&self, path: &'a Path) -> Option<Cow<'a, Path>> {
        if path.strip_prefix(&self.root).is_ok() {
            return Some(Cow::Borrowed(path));
        }
        let relative = path.strip_prefix(&self.watched_root).ok().or_else(|| {
            (!path.is_absolute() && !self.watched_root.is_absolute()).then_some(path)
        })?;
        Some(Cow::Owned(self.root.join(relative)))
    }
}

/// One file the build left out of the index.
#[derive(Debug, Clone)]
pub enum WorkspaceIndexWarning {
    /// File bytes are not valid UTF-8.
    InvalidUtf8Source {
        /// Workspace-relative path.
        path: ProjectPath,
        /// Registered failure details.
        error: Arc<RiftError>,
    },
    /// File bytes contain a NUL byte.
    BinarySource(ProjectPath),
    /// File exceeds the configured per-file byte bound.
    FileTooLarge {
        /// Workspace-relative path.
        path: ProjectPath,
        /// Registered failure details.
        error: Arc<RiftError>,
    },
    /// The syntax provider refused the file under one of its bounds. A file refused for its
    /// source size stays in the index as text the provider does not parse; see
    /// [`Self::holds_text`].
    SyntaxTooLarge {
        /// The file left out.
        path: ProjectPath,
        /// Which bound the provider's refusal names.
        error: Arc<RiftError>,
    },
    /// The Contribution contract refused one of the file's declarations.
    Contribution {
        /// The file left out.
        path: ProjectPath,
        /// The Contribution field the refusal names.
        error: Arc<RiftError>,
    },
    /// The workspace publication was full before this file's declarations were offered.
    DeclarationsBeyondBound(ProjectPath),
}

impl PartialEq for WorkspaceIndexWarning {
    fn eq(&self, other: &Self) -> bool {
        let same_error = |left: &Arc<RiftError>, right: &Arc<RiftError>| {
            left.slug() == right.slug()
                && left.detail() == right.detail()
                && left.context().collect::<Vec<_>>() == right.context().collect::<Vec<_>>()
        };
        match (self, other) {
            (
                Self::InvalidUtf8Source {
                    path: left_path,
                    error: left_error,
                },
                Self::InvalidUtf8Source {
                    path: right_path,
                    error: right_error,
                },
            )
            | (
                Self::FileTooLarge {
                    path: left_path,
                    error: left_error,
                },
                Self::FileTooLarge {
                    path: right_path,
                    error: right_error,
                },
            )
            | (
                Self::SyntaxTooLarge {
                    path: left_path,
                    error: left_error,
                },
                Self::SyntaxTooLarge {
                    path: right_path,
                    error: right_error,
                },
            )
            | (
                Self::Contribution {
                    path: left_path,
                    error: left_error,
                },
                Self::Contribution {
                    path: right_path,
                    error: right_error,
                },
            ) => left_path == right_path && same_error(left_error, right_error),
            (Self::BinarySource(left), Self::BinarySource(right))
            | (Self::DeclarationsBeyondBound(left), Self::DeclarationsBeyondBound(right)) => {
                left == right
            }
            _ => false,
        }
    }
}

impl Eq for WorkspaceIndexWarning {}

/// Outcome of reading one file into the index: parsed, or skipped with the warning
/// that names it - left out, or held as text the provider did not parse
/// ([`WorkspaceIndexWarning::holds_text`]).
pub enum IndexRead<File> {
    /// File content passed read and syntax checks.
    Included(File),
    /// Catalog or syntax bound left the file out of this read.
    Skipped(WorkspaceIndexWarning),
}

fn left_out_file(
    error: RiftError,
    path: ProjectPath,
) -> Result<Option<WorkspaceIndexWarning>, RiftError> {
    let slug = error.slug();
    match slug {
        errors::index::workspace_file_too_large::SLUG => {
            Ok(Some(WorkspaceIndexWarning::FileTooLarge {
                path,
                error: Arc::new(error),
            }))
        }
        errors::index::workspace_invalid_source::SLUG => {
            Ok(Some(WorkspaceIndexWarning::InvalidUtf8Source {
                path,
                error: Arc::new(error),
            }))
        }
        errors::index::workspace_syntax::SLUG => {
            let source = source_rift_error(&error);
            if source.is_some_and(|source| is_syntax_bound(source.slug())) {
                Ok(Some(WorkspaceIndexWarning::SyntaxTooLarge {
                    path,
                    error: Arc::new(error),
                }))
            } else {
                error.fail()
            }
        }
        errors::index::workspace_provider::SLUG => {
            let source = source_rift_error(&error);
            let Some(source) = source else {
                return error.fail();
            };
            if source.context().any(|(key, _)| key == "field") {
                Ok(Some(WorkspaceIndexWarning::Contribution {
                    path,
                    error: Arc::new(error),
                }))
            } else {
                error.fail()
            }
        }
        _ => error.fail(),
    }
}

fn source_rift_error(error: &RiftError) -> Option<&RiftError> {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(error) = current.downcast_ref::<RiftError>() {
            return Some(error);
        }
        source = std::error::Error::source(current);
    }
    None
}

fn is_syntax_bound(slug: ErrorSlug) -> bool {
    matches!(
        slug,
        errors::syntax::source_too_large::SLUG
            | errors::syntax::too_many_nodes::SLUG
            | errors::syntax::too_deep::SLUG
            | errors::syntax::too_many_captures::SLUG
            | errors::syntax::too_many_markdown_inline_ranges::SLUG
            | errors::syntax::markdown_progress_exceeded::SLUG
    )
}

/// Indexed file and syntax nodes selected for one source position.
#[derive(Debug)]
pub struct IndexedFileNodes {
    /// Parsed file used to validate source identity and position.
    pub file: IndexedFile,
    /// Nodes that cover the requested position.
    pub nodes: Vec<SyntaxNode>,
}

impl<File> IndexRead<File> {
    /// Leaves one file out, recording the warning that names it once, at the
    /// build that read it.
    fn left_out(warning: WorkspaceIndexWarning) -> Self {
        log_left_out(&warning);
        Self::Skipped(warning)
    }

    /// Holds one file as text its provider did not parse, recording the warning that
    /// names it once, at the build that read it.
    fn held_unparsed(warning: WorkspaceIndexWarning) -> Self {
        log_held_unparsed(&warning);
        Self::Skipped(warning)
    }
}

/// Records one file left out of the index, once, at the build that left it out.
fn log_left_out(warning: &WorkspaceIndexWarning) {
    rift_tracing::warn!(
        component = "index",
        operation = "index.build",
        path = warning.path().as_str(),
        reason = %warning.reason(),
        "file left out of the index"
    );
}

/// Records one file the index holds as text its provider did not parse, once, at the
/// build that read it, with the fields a left-out file's record carries.
fn log_held_unparsed(warning: &WorkspaceIndexWarning) {
    rift_tracing::warn!(
        component = "index",
        operation = "index.build",
        path = warning.path().as_str(),
        reason = %warning.reason(),
        "file held unparsed in the index"
    );
}

/// What the index keeps of a file it read and left out: the two digests a capture of
/// the tree and a watcher observation compare against, and nothing it could serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeftOutFileState {
    /// Digest of the bytes alone, what `WorkspaceIndex::digest` answers.
    content: FileDigest,
    /// Digest of the bytes and the executable bit, what `WorkspaceIndex::digests` answers.
    state: FileDigest,
    /// Number of bytes counted against `workspace_bytes_max`.
    bytes: usize,
}

impl LeftOutFileState {
    fn of(file: &TextSourceFile) -> Self {
        Self {
            content: file.digest(),
            state: FileDigest::of_file_state(file.content().as_bytes(), file.executable()),
            bytes: file.content().len(),
        }
    }

    fn of_indexed(file: &IndexedFile) -> Self {
        Self {
            content: file.digest(),
            state: FileDigest::of_file_state(file.source().as_bytes(), file.executable()),
            bytes: file.source().len(),
        }
    }
}

/// The files one build holds and the files it left out, gathered before the index is
/// assembled over them.
#[derive(Clone, Default)]
pub(crate) struct IndexContents {
    files: BTreeMap<ProjectPath, Arc<IndexedFile>>,
    text_files: BTreeMap<ProjectPath, Arc<TextSourceFile>>,
    left_out: BTreeMap<ProjectPath, LeftOutFileState>,
    warnings: Vec<WorkspaceIndexWarning>,
}

/// Project-relative paths selected for one workspace map.
#[derive(Default)]
pub struct WorkspaceMapPaths {
    /// Source paths and their selected language.
    pub source: Vec<(ProjectPath, rift_protocol::read::Language)>,
    /// Baseline text paths, excluding lockfiles.
    pub text: Vec<ProjectPath>,
}

/// One cancellable workspace preparation, retaining every completed file for later
/// immutable snapshots.
pub struct WorkspaceIndexPreparation {
    root: PathBuf,
    limits: WorkspaceIndexLimits,
    composition: ProviderComposition,
    language: Arc<WorkspaceLanguagePolicy>,
    text_inclusion: TextFileInclusion,
    discovered: Option<DiscoveredPaths>,
    contents: IndexContents,
    content_cache: WorkspaceContentCache,
    workspace_bytes: usize,
    source_cursor: usize,
    text_cursor: usize,
    lockfile_cursor: usize,
}

impl fmt::Debug for WorkspaceIndexPreparation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceIndexPreparation")
            .field("root", &self.root)
            .field("prepared", &self.prepared())
            .field("total", &self.total())
            .finish_non_exhaustive()
    }
}

type PreparedBatch = Result<usize, (usize, RiftError)>;

impl WorkspaceIndexPreparation {
    /// Creates empty immutable views before discovery starts.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid root, configuration, or composition.
    pub fn new(
        root: &Path,
        limits: WorkspaceIndexLimits,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
    ) -> Result<Self, RiftError> {
        let root = canonical_root(root)?;
        let composition = composition()?;
        let language = Arc::new(WorkspaceLanguagePolicy::build(
            &root,
            languages,
            text_inclusion,
        )?);
        Ok(Self {
            root,
            limits,
            composition,
            language,
            text_inclusion: text_inclusion.clone(),
            discovered: None,
            contents: IndexContents::default(),
            content_cache: WorkspaceContentCache::default(),
            workspace_bytes: 0,
            source_cursor: 0,
            text_cursor: 0,
            lockfile_cursor: 0,
        })
    }

    /// Uses shared content and syntax facts for matching source files.
    #[must_use]
    pub fn with_content_cache(mut self, cache: WorkspaceContentCache) -> Self {
        self.content_cache = cache;
        self
    }

    /// Builds an empty index under the accepted workspace configuration.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the empty index cannot be assembled.
    pub fn empty_snapshot(&self) -> Result<WorkspaceIndex, RiftError> {
        WorkspaceIndex::from_parts(
            self.root.clone(),
            IndexContents::default(),
            self.composition.clone(),
            self.limits,
            Arc::clone(&self.language),
            self.text_inclusion.clone(),
            self.content_cache.clone(),
            None,
        )
    }

    /// Discovers the complete selected file set under the same visibility authority as a
    /// full build.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for discovery, configured bounds, or cancellation.
    ///
    /// # Panics
    ///
    /// Panics if discovery was already run. A preparation uses one visibility policy.
    pub fn discover(
        &mut self,
        visibility: &SourceVisibility,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<usize, RiftError> {
        assert!(
            self.discovered.is_none(),
            "discovery runs once per preparation"
        );
        self.discovered = Some(discover_cancellable(
            &self.root,
            self.limits,
            visibility,
            &self.language,
            cancelled,
        )?);
        Ok(self.total().expect("discovery has completed"))
    }

    /// Next bounded publication count: the first completed batch, then doubled counts,
    /// ending exactly at the selected file total.
    #[must_use]
    pub fn next_checkpoint(&self) -> Option<usize> {
        let total = self.total()?;
        let prepared = self.prepared();
        if prepared == total {
            return None;
        }
        let next = if prepared == 0 {
            SOURCE_BATCH_FILES
        } else {
            prepared.saturating_mul(2)
        };
        Some(next.min(total))
    }

    /// Number of selected visible files, once discovery has completed.
    #[must_use]
    pub fn total(&self) -> Option<usize> {
        self.discovered.as_ref().and_then(|paths| {
            paths
                .source
                .len()
                .checked_add(paths.text.len())?
                .checked_add(paths.lockfiles.len())
        })
    }

    /// Selected project-relative paths, once discovery has completed.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a discovered path cannot be made project-relative.
    pub fn selected_paths(&self) -> Result<Option<Vec<ProjectPath>>, RiftError> {
        let Some(total) = self.total() else {
            return Ok(None);
        };
        self.paths_through(total).map(Some)
    }

    /// Number of selected files whose capture and analysis have finished.
    #[must_use]
    pub const fn prepared(&self) -> usize {
        self.source_cursor
            .saturating_add(self.text_cursor)
            .saturating_add(self.lockfile_cursor)
    }

    /// Project-relative paths whose capture and analysis have finished.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a discovered path cannot be made project-relative.
    pub fn prepared_paths(&self) -> Result<Vec<ProjectPath>, RiftError> {
        self.paths_through(self.prepared())
    }

    /// Replaces prepared file contents with the result of an incremental rebuild of this
    /// preparation's snapshot. Discovery and completed path cursors remain unchanged.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `index` has a different root or bounds.
    pub fn retain_rebuilt_snapshot(&mut self, index: &WorkspaceIndex) {
        debug_assert_eq!(self.root, index.root);
        debug_assert_eq!(self.limits, index.limits);
        self.contents = IndexContents::from_index(index);
        self.workspace_bytes = WorkspaceIndex::indexed_bytes(
            &self.contents.files,
            &self.contents.text_files,
            &self.contents.left_out,
        );
    }

    /// Captures the paths this preparation has finished reading, reusing the previous
    /// capture only when the retained file stat and capture boundary still match.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a prepared path cannot be read within limits.
    pub fn capture_prepared_paths(
        &self,
        last: &LastCapture,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(WorkspaceDigests, LastCapture), RiftError> {
        let Some(paths) = &self.discovered else {
            return Ok((WorkspaceDigests::default(), LastCapture::default()));
        };
        let mut remaining = self.prepared();
        let source_count = remaining.min(paths.source.len());
        remaining -= source_count;
        let text_count = remaining.min(paths.text.len());
        remaining -= text_count;
        let lockfile_count = remaining.min(paths.lockfiles.len());
        let source = paths.source[..source_count]
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let other = paths.text[..text_count]
            .iter()
            .chain(&paths.lockfiles[..lockfile_count])
            .cloned()
            .collect::<Vec<_>>();
        capture_path_lists_with_boundary(
            &self.root,
            self.limits,
            &CapturePathLists {
                source: &source,
                other: &other,
                last,
                boundary: CaptureBoundary::create(&self.root),
                root_identity: root_identity(&self.root),
                cancelled,
            },
        )
    }

    /// Project-relative paths whose completed reads form this snapshot, split by index class.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a discovered path cannot be made project-relative.
    pub fn prepared_path_classes(&self) -> Result<(Vec<ProjectPath>, Vec<ProjectPath>), RiftError> {
        let Some(paths) = &self.discovered else {
            return Ok((Vec::new(), Vec::new()));
        };
        let mut remaining = self.prepared();
        let source_count = remaining.min(paths.source.len());
        remaining -= source_count;
        let text_count = remaining.min(paths.text.len());
        remaining -= text_count;
        let lockfile_count = remaining.min(paths.lockfiles.len());
        let project_path = |path: &PathBuf| {
            let relative = path.strip_prefix(&self.root).map_err(|error| {
                errors::index::workspace_invalid_path()
                    .path(path)
                    .source(error)
                    .error()
            })?;
            relative_path(relative)
        };
        let source = paths.source[..source_count]
            .iter()
            .map(|(path, _)| project_path(path))
            .collect::<Result<Vec<_>, _>>()?;
        let other = paths.text[..text_count]
            .iter()
            .chain(&paths.lockfiles[..lockfile_count])
            .map(project_path)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((source, other))
    }

    /// Project-relative paths discovered under this preparation's visibility policy, split
    /// into source files and baseline text files. Excluded lockfiles are omitted because the
    /// workspace map does not count them as indexed files.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a discovered path cannot be made project-relative.
    pub fn discovered_map_paths(&self) -> Result<WorkspaceMapPaths, RiftError> {
        let Some(paths) = &self.discovered else {
            return Ok(WorkspaceMapPaths::default());
        };
        let project_path = |path: &Path| {
            let relative = path.strip_prefix(&self.root).map_err(|error| {
                errors::index::workspace_invalid_path()
                    .path(path)
                    .source(error)
                    .error()
            })?;
            relative_path(relative)
        };
        let source = paths
            .source
            .iter()
            .map(|(path, provider)| Ok((project_path(path)?, provider.language().clone())))
            .collect::<Result<Vec<_>, RiftError>>()?;
        let text = paths
            .text
            .iter()
            .map(|path| project_path(path))
            .collect::<Result<Vec<_>, RiftError>>()?;
        Ok(WorkspaceMapPaths { source, text })
    }

    fn paths_through(&self, count: usize) -> Result<Vec<ProjectPath>, RiftError> {
        let Some(paths) = &self.discovered else {
            return Ok(Vec::new());
        };
        let mut remaining = count;
        let source_count = remaining.min(paths.source.len());
        remaining -= source_count;
        let text_count = remaining.min(paths.text.len());
        remaining -= text_count;
        let lockfile_count = remaining.min(paths.lockfiles.len());
        paths.source[..source_count]
            .iter()
            .map(|(path, _)| path)
            .chain(paths.text[..text_count].iter())
            .chain(paths.lockfiles[..lockfile_count].iter())
            .map(|path| {
                let relative = path.strip_prefix(&self.root).map_err(|error| {
                    errors::index::workspace_invalid_path()
                        .path(path)
                        .source(error)
                        .error()
                })?;
                relative_path(relative)
            })
            .collect()
    }

    /// Reads through `target` selected files and returns a new index over all completed
    /// files. Prior files are shared through their `Arc` values and are not parsed again.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an invalid target, read, syntax, bound, or
    /// cancellation.
    ///
    /// # Panics
    ///
    /// Panics before discovery or when `target` moves backward or exceeds selected files.
    pub fn advance_to(
        &mut self,
        target: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<WorkspaceIndex, RiftError> {
        self.advance_to_with_previous(target, None, cancelled)
    }

    /// Reads through `target` selected files and reuses derived data from `previous` when
    /// both indexes use the same accepted inputs.
    ///
    /// A mismatch skips reuse and assembles the same result as `advance_to`.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an invalid target, read, syntax, bound, or
    /// cancellation.
    ///
    /// # Panics
    ///
    /// Panics before discovery or when `target` moves backward or exceeds selected files.
    pub fn advance_to_with_previous(
        &mut self,
        target: usize,
        previous: Option<&WorkspaceIndex>,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<WorkspaceIndex, RiftError> {
        let paths = self.discovered.as_ref().unwrap_or_else(|| {
            unreachable!("workspace files cannot be prepared before discovery completes")
        });
        let total = paths
            .source
            .len()
            .checked_add(paths.text.len())
            .and_then(|count| count.checked_add(paths.lockfiles.len()))
            .expect("discovered file count must fit usize");
        assert!(
            (self.prepared()..=total).contains(&target),
            "preparation target must not move backward or exceed selected files"
        );
        let paths = self.discovered.take().expect("discovery was checked above");
        let advanced = (|| {
            while self.prepared() < target {
                check_cancelled(cancelled)?;
                if self.source_cursor < paths.source.len() {
                    self.advance_sources(&paths, target, cancelled)?;
                } else if self.text_cursor < paths.text.len() {
                    self.advance_text(&paths, target, cancelled)?;
                } else {
                    self.advance_lockfiles(&paths, target, cancelled)?;
                }
            }
            self.snapshot(previous)
        })();
        self.discovered = Some(paths);
        advanced
    }

    fn advance_sources(
        &mut self,
        paths: &DiscoveredPaths,
        target: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RiftError> {
        let end = self.batch_end(self.source_cursor, paths.source.len(), target);
        let completed = self.contents.hold_parsed_sources_progress(
            &self.root,
            &paths.source[self.source_cursor..end],
            self.limits,
            &mut self.workspace_bytes,
            None,
            &self.content_cache,
            cancelled,
        );
        self.source_cursor += batch_completed(&completed);
        completed.map(|_| ()).map_err(|(_, error)| error)
    }

    fn advance_text(
        &mut self,
        paths: &DiscoveredPaths,
        target: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RiftError> {
        let end = self.batch_end(self.text_cursor, paths.text.len(), target);
        let completed = self.contents.hold_read_texts_progress(
            &self.root,
            &paths.text[self.text_cursor..end],
            self.limits,
            &mut self.workspace_bytes,
            cancelled,
        );
        self.text_cursor += batch_completed(&completed);
        completed.map(|_| ()).map_err(|(_, error)| error)
    }

    fn advance_lockfiles(
        &mut self,
        paths: &DiscoveredPaths,
        target: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RiftError> {
        let end = self.batch_end(self.lockfile_cursor, paths.lockfiles.len(), target);
        let completed = self.contents.hold_read_lockfiles_progress(
            &self.root,
            &paths.lockfiles[self.lockfile_cursor..end],
            self.limits,
            &mut self.workspace_bytes,
            cancelled,
        );
        self.lockfile_cursor += batch_completed(&completed);
        completed.map(|_| ()).map_err(|(_, error)| error)
    }

    fn batch_end(&self, cursor: usize, length: usize, target: usize) -> usize {
        cursor
            .saturating_add(SOURCE_BATCH_FILES)
            .min(length)
            .min(cursor.saturating_add(target.saturating_sub(self.prepared())))
    }

    fn snapshot(&self, previous: Option<&WorkspaceIndex>) -> Result<WorkspaceIndex, RiftError> {
        WorkspaceIndex::from_parts(
            self.root.clone(),
            self.contents.clone(),
            self.composition.clone(),
            self.limits,
            Arc::clone(&self.language),
            self.text_inclusion.clone(),
            self.content_cache.clone(),
            previous.filter(|index| self.accepts_previous(index)),
        )
    }

    fn accepts_previous(&self, previous: &WorkspaceIndex) -> bool {
        self.root == previous.root
            && self.limits == previous.limits
            && self.text_inclusion == previous.text_inclusion
            && Arc::ptr_eq(&self.language, &previous.language)
            && self.composition.id() == previous.composition.id()
            && self.composition.steps() == previous.composition.steps()
    }
}

fn batch_completed(result: &PreparedBatch) -> usize {
    result
        .as_ref()
        .map_or_else(|(count, _)| *count, |count| *count)
}

impl IndexContents {
    fn from_index(index: &WorkspaceIndex) -> Self {
        Self {
            files: index.files.clone(),
            text_files: index.text_files.clone(),
            left_out: index.left_out.clone(),
            warnings: index.warnings.clone(),
        }
    }

    /// The previous index's contents, less every path `changes` names: a rebuild reads
    /// those again, and a warning the previous index carried for one of them goes with it.
    fn carried_from(index: &WorkspaceIndex, changes: &PathChanges) -> Self {
        let touched: BTreeSet<&ProjectPath> = changes.paths().collect();
        let mut contents = Self {
            files: index.files.clone(),
            text_files: index.text_files.clone(),
            left_out: index.left_out.clone(),
            warnings: index
                .warnings
                .iter()
                .filter(|warning| !touched.contains(warning.path()))
                .cloned()
                .collect(),
        };
        for path in changes.paths() {
            contents.files.remove(path);
            contents.text_files.remove(path);
            contents.left_out.remove(path);
        }
        contents
    }

    /// Files this build holds so far, syntax-indexed or text alone; every held file
    /// sits in `text_files`.
    pub(crate) fn held_count(&self) -> usize {
        self.text_files.len()
    }

    /// Holds one cataloged file a provider claims: in both maps when its syntax facts fit
    /// the provider's bounds. A file the provider refuses under one of its bounds keeps
    /// only its digests, so a capture of the tree still agrees with the index, and
    /// `warnings` names it.
    pub(crate) fn hold_source_file(
        &mut self,
        text_file: TextSourceFile,
        context_path: &Path,
        provider: &dyn SyntaxProvider,
        syntax: SyntaxLimits,
    ) -> Result<(), RiftError> {
        self.hold_source_file_with_cache(
            text_file,
            context_path,
            provider,
            syntax,
            &WorkspaceContentCache::default(),
            WORKSPACE_FILES_MAX_DEFAULT,
        )
    }

    fn hold_source_file_with_cache(
        &mut self,
        mut text_file: TextSourceFile,
        context_path: &Path,
        provider: &dyn SyntaxProvider,
        syntax_limits: SyntaxLimits,
        cache: &WorkspaceContentCache,
        entries_max: usize,
    ) -> Result<(), RiftError> {
        let digest = text_file.digest();
        let (cached_source, cached_syntax) = cache.get(digest, syntax_limits, provider);
        if let Some(source) = cached_source {
            text_file.content = source;
        }
        let parsed = if let Some(syntax) = cached_syntax {
            IndexRead::Included(Arc::new(IndexedFile::new_with_shared_syntax(
                text_file.path().clone(),
                Arc::clone(&text_file.content),
                digest,
                text_file.executable(),
                syntax,
            )))
        } else {
            match shared_read(syntax_read(
                &text_file,
                context_path,
                provider,
                syntax_limits,
            )?) {
                IndexRead::Included(file) => IndexRead::Included(cache_indexed_file(
                    file,
                    &mut text_file,
                    provider,
                    syntax_limits,
                    cache,
                    entries_max,
                )),
                IndexRead::Skipped(warning) => IndexRead::Skipped(warning),
            }
        };
        self.hold_parsed_source(text_file, parsed);
        Ok(())
    }

    /// Holds one cataloged file with the syntax outcome already read for it. A file the
    /// provider refused for its source size alone is held as text, since its text still
    /// answers search; any other refusal keeps only its digests.
    fn hold_parsed_source(
        &mut self,
        text_file: TextSourceFile,
        parsed: IndexRead<Arc<IndexedFile>>,
    ) {
        match parsed {
            IndexRead::Included(file) => {
                self.files.insert(file.path().clone(), file);
                self.hold_text_file(text_file);
            }
            IndexRead::Skipped(warning) if warning.holds_text() => {
                self.hold_text_file(text_file);
                self.warnings.push(warning);
            }
            IndexRead::Skipped(warning) => {
                self.left_out
                    .insert(text_file.path().clone(), LeftOutFileState::of(&text_file));
                self.warnings.push(warning);
            }
        }
    }

    /// Reads and parses every source path a provider claimed, in walk order.
    ///
    /// Each batch of [`SOURCE_BATCH_FILES`] paths is read and parsed across the rayon
    /// pool, then held in walk order: `workspace_bytes` counts each kept file before its
    /// parse outcome is held, so the `workspace_bytes_max` refusal and every other failure
    /// name the path a sequential read would name. The refusal stops the build after the
    /// batch that crossed it, so at most one batch is read past the bound.
    #[expect(
        clippy::too_many_arguments,
        reason = "Inputs carry one bounded source-read operation through the batch."
    )]
    fn hold_parsed_sources(
        &mut self,
        root: &Path,
        sources: &[(PathBuf, &'static dyn SyntaxProvider)],
        limits: WorkspaceIndexLimits,
        workspace_bytes: &mut usize,
        previous: Option<&WorkspaceIndex>,
        content_cache: &WorkspaceContentCache,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RiftError> {
        for batch in sources.chunks(SOURCE_BATCH_FILES) {
            self.hold_parsed_sources_progress(
                root,
                batch,
                limits,
                workspace_bytes,
                previous,
                content_cache,
                cancelled,
            )
            .map_err(|(_, error)| error)?;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Inputs carry one bounded source-read operation through the batch."
    )]
    fn hold_parsed_sources_progress(
        &mut self,
        root: &Path,
        sources: &[(PathBuf, &'static dyn SyntaxProvider)],
        limits: WorkspaceIndexLimits,
        workspace_bytes: &mut usize,
        previous: Option<&WorkspaceIndex>,
        content_cache: &WorkspaceContentCache,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> PreparedBatch {
        let read: Vec<Result<IndexRead<ParsedSource>, RiftError>> = sources
            .par_iter()
            .map(|(path, provider)| {
                check_cancelled(cancelled)?;
                ParsedSource::read(root, path, *provider, limits, previous, content_cache)
            })
            .collect();
        let mut completed = 0;
        for ((path, _), read) in sources.iter().zip(read) {
            if let Err(error) = check_cancelled(cancelled) {
                return Err((completed, error));
            }
            let read = match read {
                Ok(read) => read,
                Err(error) => return Err((completed, error)),
            };
            match read {
                IndexRead::Included(ParsedSource { text_file, parsed }) => {
                    let length = text_file.content().len();
                    let bytes_before_file = *workspace_bytes;
                    if let Err(error) = count_workspace_bytes(workspace_bytes, length, path, limits)
                    {
                        return Err((completed, error));
                    }
                    let parsed = match parsed {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            *workspace_bytes = bytes_before_file;
                            return Err((completed, error));
                        }
                    };
                    self.hold_parsed_source(text_file, parsed);
                }
                IndexRead::Skipped(warning) => self.leave_out(warning),
            }
            completed += 1;
        }
        Ok(completed)
    }

    /// Reads every text path, in walk order, batched across the rayon pool the way
    /// [`Self::hold_parsed_sources`] batches sources.
    fn hold_read_texts(
        &mut self,
        root: &Path,
        texts: &[PathBuf],
        limits: WorkspaceIndexLimits,
        workspace_bytes: &mut usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RiftError> {
        for batch in texts.chunks(SOURCE_BATCH_FILES) {
            self.hold_read_texts_progress(root, batch, limits, workspace_bytes, cancelled)
                .map_err(|(_, error)| error)?;
        }
        Ok(())
    }

    fn hold_read_texts_progress(
        &mut self,
        root: &Path,
        texts: &[PathBuf],
        limits: WorkspaceIndexLimits,
        workspace_bytes: &mut usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> PreparedBatch {
        let read: Vec<Result<IndexRead<TextSourceFile>, RiftError>> = texts
            .par_iter()
            .map(|path| {
                check_cancelled(cancelled)?;
                catalog_file(root, path, limits)
            })
            .collect();
        let mut completed = 0;
        for (path, read) in texts.iter().zip(read) {
            if let Err(error) = check_cancelled(cancelled) {
                return Err((completed, error));
            }
            let read = match read {
                Ok(read) => read,
                Err(error) => return Err((completed, error)),
            };
            match read {
                IndexRead::Included(text_file) => {
                    let length = text_file.content().len();
                    if let Err(error) = count_workspace_bytes(workspace_bytes, length, path, limits)
                    {
                        return Err((completed, error));
                    }
                    self.hold_text_file(text_file);
                }
                IndexRead::Skipped(warning) => self.leave_out(warning),
            }
            completed += 1;
        }
        Ok(completed)
    }

    /// Reads every lockfile search leaves out, in walk order, keeping each one's digests
    /// alone: the bytes count against `workspace_bytes_max` exactly as a text file's do,
    /// so a request-time capture of the tree agrees with the index.
    fn hold_read_lockfiles(
        &mut self,
        root: &Path,
        lockfiles: &[PathBuf],
        limits: WorkspaceIndexLimits,
        workspace_bytes: &mut usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RiftError> {
        self.hold_read_lockfiles_progress(root, lockfiles, limits, workspace_bytes, cancelled)
            .map(|_| ())
            .map_err(|(_, error)| error)
    }

    fn hold_read_lockfiles_progress(
        &mut self,
        root: &Path,
        lockfiles: &[PathBuf],
        limits: WorkspaceIndexLimits,
        workspace_bytes: &mut usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> PreparedBatch {
        let mut completed = 0;
        for path in lockfiles {
            if let Err(error) = check_cancelled(cancelled) {
                return Err((completed, error));
            }
            match catalog_file(root, path, limits) {
                Ok(IndexRead::Included(text_file)) => {
                    let length = text_file.content().len();
                    if let Err(error) = count_workspace_bytes(workspace_bytes, length, path, limits)
                    {
                        return Err((completed, error));
                    }
                    self.hold_lockfile(&text_file);
                }
                Ok(IndexRead::Skipped(warning)) => self.leave_out(warning),
                Err(error) => return Err((completed, error)),
            }
            completed += 1;
        }
        Ok(completed)
    }

    /// Keeps one lockfile search leaves out by its digests alone: the index serves nothing
    /// from it, and the capture and the dependency context still see it move.
    pub(crate) fn hold_lockfile(&mut self, text_file: &TextSourceFile) {
        self.left_out
            .insert(text_file.path().clone(), LeftOutFileState::of(text_file));
    }

    /// Holds one cataloged file no provider claims.
    pub(crate) fn hold_text_file(&mut self, text_file: TextSourceFile) {
        self.text_files
            .insert(text_file.path().clone(), Arc::new(text_file));
    }

    /// Records one file the catalog read left out.
    pub(crate) fn leave_out(&mut self, warning: WorkspaceIndexWarning) {
        self.warnings.push(warning);
    }

    /// Leaves one held source file out after its declarations were refused: the file
    /// leaves both maps and keeps only its digests, and `warnings` names it. Answers
    /// whether `path` named a held file.
    fn leave_out_held(&mut self, path: &ProjectPath, warning: WorkspaceIndexWarning) -> bool {
        let Some(file) = self.files.remove(path) else {
            return false;
        };
        self.text_files.remove(path);
        self.left_out
            .insert(path.clone(), LeftOutFileState::of_indexed(&file));
        log_left_out(&warning);
        self.warnings.push(warning);
        self.sort_warnings();
        true
    }

    /// The same contents with the warnings in project-path order.
    fn sorted(mut self) -> Self {
        self.sort_warnings();
        self
    }

    fn sort_warnings(&mut self) {
        self.warnings
            .sort_by(|left, right| left.path().cmp(right.path()));
    }
}

impl WorkspaceIndexWarning {
    /// Whether the file keeps its text in the index: the provider refused its source for
    /// its size alone, so the file is held as text no declaration was extracted from.
    #[must_use]
    pub fn holds_text(&self) -> bool {
        matches!(
            self,
            Self::SyntaxTooLarge { error, .. }
                if std::error::Error::source(error.as_ref()).is_some_and(|source| {
                    source.downcast_ref::<RiftError>().is_some_and(|source| {
                        source.slug() == errors::syntax::source_too_large::SLUG
                    })
                })
        )
    }

    /// The file this warning names.
    #[must_use]
    pub fn path(&self) -> &ProjectPath {
        match self {
            Self::InvalidUtf8Source { path, .. }
            | Self::BinarySource(path)
            | Self::FileTooLarge { path, .. }
            | Self::SyntaxTooLarge { path, .. }
            | Self::Contribution { path, .. }
            | Self::DeclarationsBeyondBound(path) => path,
        }
    }

    /// Why the file is absent from the index, as the clause that follows its
    /// path.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::InvalidUtf8Source { .. } => "holds bytes that are not valid UTF-8".to_owned(),
            Self::BinarySource(_) => "contains a NUL byte".to_owned(),
            Self::FileTooLarge { error, .. }
            | Self::SyntaxTooLarge { error, .. }
            | Self::Contribution { error, .. } => error.detail(),
            Self::DeclarationsBeyondBound(_) => {
                format!(
                    "stands past the declarations the index holds ({SOURCE_DECLARATIONS_FIELD})"
                )
            }
        }
    }
}

/// Immutable current-workspace Rust read index.
///
/// Files are keyed by project path and held behind `Arc`, so the next publication can
/// replace the entries one change set names and share every other file with this one.
/// A reader still retains one complete, immutable index.
#[derive(Debug)]
pub struct WorkspaceIndex {
    root: PathBuf,
    files: BTreeMap<ProjectPath, Arc<IndexedFile>>,
    text_files: BTreeMap<ProjectPath, Arc<TextSourceFile>>,
    left_out: BTreeMap<ProjectPath, LeftOutFileState>,
    composition: ProviderComposition,
    limits: WorkspaceIndexLimits,
    language: Arc<WorkspaceLanguagePolicy>,
    content_cache: WorkspaceContentCache,
    symbol_documents: RwLock<BTreeMap<ProjectPath, CachedSymbolDocuments>>,
    text_inclusion: TextFileInclusion,
    fingerprint: WorkspaceFingerprint,
    semantics: WorkspaceSemantics,
    documentation: Arc<DocumentationCollection>,
    /// The documentation layer over `documentation`, built by the first read that projects
    /// onto it; the next publication is a new index and builds its own.
    documentation_layer: OnceLock<Result<DocumentationLayer<'static>, RiftError>>,
    /// The held text files holding a line longer than `[search.text] max_chunk`, found by
    /// the first pattern search that asks; the next publication is a new index and finds
    /// its own.
    split_line_files: OnceLock<BTreeSet<ProjectPath>>,
    notebooks: NotebookFiles,
    warnings: Vec<WorkspaceIndexWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SymbolDocumentKey {
    digest: FileDigest,
    language: String,
    source_bytes_max: usize,
    syntax_nodes_max: usize,
    syntax_depth_max: usize,
}

#[derive(Debug, Clone)]
struct CachedSymbolDocuments {
    key: SymbolDocumentKey,
    documents: Arc<[IndexDocument]>,
}

impl WorkspaceIndex {
    /// Scans visible regular files below workspace root.
    ///
    /// Hard floor, ignore rules, and source policy apply once. Every valid UTF-8
    /// file without a NUL byte enters baseline content catalog. Registered providers add
    /// syntax facts to same file identity.
    ///
    /// # Errors
    ///
    /// Returns `RiftError` for invalid root, I/O, syntax, invalid
    /// source pattern, or exceeded workspace bound.
    pub fn build(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
    ) -> Result<Self, RiftError> {
        Self::build_with_languages(
            root,
            limits,
            visibility,
            text_inclusion,
            &LanguageFileSelections::default(),
        )
    }

    /// Scans visible regular files using configured language entries.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid configuration, I/O, syntax,
    /// or exceeded workspace bounds.
    pub fn build_with_languages(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
    ) -> Result<Self, RiftError> {
        Self::build_with_languages_cancellable(
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
            &|| false,
        )
    }

    /// Scans visible regular files with configured language entries, checking `cancelled`
    /// between discovered and parsed files.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid configuration, I/O, syntax,
    /// exceeded bounds, or cancellation.
    pub fn build_with_languages_cancellable(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        Self::build_with_languages_cancellable_and_cache(
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
            &WorkspaceContentCache::default(),
            cancelled,
        )
    }

    /// Scans visible regular files with shared content and syntax facts from `cache`.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the root, bounds, file contents, or cancellation
    /// prevents the scan from completing.
    pub fn build_with_languages_cancellable_and_cache(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
        cache: &WorkspaceContentCache,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let root = canonical_root(root)?;
        let language = Arc::new(WorkspaceLanguagePolicy::build(
            &root,
            languages,
            text_inclusion,
        )?);
        Self::scanned(
            root,
            limits,
            visibility,
            language,
            text_inclusion,
            None,
            cache,
            cancelled,
        )
    }

    /// Builds the next index from a whole scan of the tree under `visibility`, sharing every
    /// source file whose bytes and executable bit this index already parsed.
    ///
    /// A folder created or removed, a directory renamed, or an ignore file rewritten names
    /// no set of files a rebuild could read alone, so the whole tree is walked and every
    /// visible file read and digested again, under the visibility the ignore files decide
    /// now. Parsing is what the scan saves: a file this index parsed from the same bytes is
    /// shared, not parsed again. The caller keeps this index's language entries, bounds,
    /// and text selection, so it rescans only when index-owned configuration is unchanged;
    /// a configuration change takes a full build instead.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for I/O, syntax, or an exceeded bound, exactly as a
    /// full build does.
    pub fn rescanned(&self, visibility: &SourceVisibility) -> Result<Self, RiftError> {
        self.rescanned_cancellable(visibility, &|| false)
    }

    /// Rescans the workspace, checking `cancelled` between discovered and parsed files.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for I/O, syntax, exceeded bounds, or cancellation.
    pub fn rescanned_cancellable(
        &self,
        visibility: &SourceVisibility,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        Self::scanned(
            self.root.clone(),
            self.limits,
            visibility,
            Arc::clone(&self.language),
            &self.text_inclusion,
            Some(self),
            &self.content_cache,
            cancelled,
        )
    }

    /// Walks, reads, and parses every visible file under `root`, sharing what `previous`
    /// parsed from the same bytes.
    #[expect(
        clippy::too_many_arguments,
        reason = "Inputs define one workspace scan and its cancellation boundary."
    )]
    fn scanned(
        root: PathBuf,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        language: Arc<WorkspaceLanguagePolicy>,
        text_inclusion: &TextFileInclusion,
        previous: Option<&Self>,
        content_cache: &WorkspaceContentCache,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        check_cancelled(cancelled)?;
        let composition = composition()?;
        let classified =
            rift_tracing::traced!(component = "index", operation = "index.discover", {
                discover_cancellable(&root, limits, visibility, &language, cancelled)
            })?;
        let BuiltContents {
            files,
            text_files,
            left_out,
            warnings,
            fingerprint,
            semantics,
        } = rift_tracing::traced!(component = "index", operation = "index.parse", {
            let mut workspace_bytes = 0_usize;
            let mut contents = IndexContents::default();
            contents.hold_parsed_sources(
                &root,
                &classified.source,
                limits,
                &mut workspace_bytes,
                previous,
                content_cache,
                cancelled,
            )?;
            contents.hold_read_texts(
                &root,
                &classified.text,
                limits,
                &mut workspace_bytes,
                cancelled,
            )?;
            contents.hold_read_lockfiles(
                &root,
                &classified.lockfiles,
                limits,
                &mut workspace_bytes,
                cancelled,
            )?;
            check_cancelled(cancelled)?;
            built_contents(
                &root,
                contents.sorted(),
                limits.declarations_max(),
                previous.map(|index| index.semantics.graph()),
            )
        })?;
        let declarations = crate::documentation::declarations(&files, &semantics);
        let (documentation, notebooks) = crate::documentation::build(
            &files,
            &text_files,
            &declarations,
            &documentation_selection(text_inclusion)?,
            checked_chunk_bytes_max(text_inclusion.chunk_bytes_max()),
            previous.map(|index| (&*index.documentation, &index.notebooks)),
        )?;
        let symbol_documents = carried_symbol_documents(previous, &files, limits.syntax());
        Ok(Self {
            root,
            files,
            text_files,
            left_out,
            composition,
            limits,
            language,
            content_cache: content_cache.clone(),
            symbol_documents,
            text_inclusion: text_inclusion.clone(),
            fingerprint,
            semantics,
            documentation: Arc::new(documentation),
            documentation_layer: OnceLock::new(),
            split_line_files: OnceLock::new(),
            notebooks,
            warnings,
        })
    }

    /// The file this index parsed from the same bytes, with the same executable bit, at
    /// the same path, when there is one.
    fn parsed_as(&self, text_file: &TextSourceFile) -> Option<Arc<IndexedFile>> {
        let held = self.files.get(text_file.path())?;
        let same_bytes = held.digest() == text_file.digest();
        let same_mode = held.executable() == text_file.executable();
        (same_bytes && same_mode).then(|| Arc::clone(held))
    }

    /// Builds the next index by reading only the paths `changes` names, sharing every
    /// other file with this one.
    ///
    /// The caller resolved `changes` against this index's own digests under the same
    /// visibility policy this index was built with, so a path here is already one the
    /// workspace includes; a configuration change takes a full rebuild and the whole scan
    /// instead. Work is one read and one parse per named path plus one map clone,
    /// against a whole scan's read and parse of every visible file.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for I/O, syntax, or an exceeded bound, exactly as a
    /// whole scan does. A named path the filesystem no longer holds is dropped rather than
    /// refused: the observation that named it has already been superseded by the deletion.
    /// A named path whose bytes are not UTF-8 is dropped the same way a whole scan drops
    /// one, and a warning naming it replaces any warning the previous index carried for
    /// that path.
    pub fn rebuilt(&self, changes: &PathChanges) -> Result<Self, RiftError> {
        self.rebuilt_cancellable(changes, &|| false)
    }

    /// Builds the named-path index, checking `cancelled` between paths.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for I/O, syntax, exceeded bounds, or cancellation.
    pub fn rebuilt_cancellable(
        &self,
        changes: &PathChanges,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let mut contents = IndexContents::carried_from(self, changes);
        let mut workspace_bytes =
            Self::indexed_bytes(&contents.files, &contents.text_files, &contents.left_out);
        for path in changes.indexed() {
            check_cancelled(cancelled)?;
            self.read_indexed_path(path, &mut contents, &mut workspace_bytes)?;
        }
        check_cancelled(cancelled)?;
        let BuiltContents {
            files,
            text_files,
            left_out,
            warnings,
            fingerprint,
            semantics,
        } = built_contents(
            &self.root,
            contents.sorted(),
            self.limits.declarations_max(),
            Some(self.semantics.graph()),
        )?;
        let declarations = crate::documentation::declarations(&files, &semantics);
        let (documentation, notebooks) = crate::documentation::build(
            &files,
            &text_files,
            &declarations,
            &documentation_selection(&self.text_inclusion)?,
            checked_chunk_bytes_max(self.text_inclusion.chunk_bytes_max()),
            Some((&self.documentation, &self.notebooks)),
        )?;
        let symbol_documents = carried_symbol_documents(Some(self), &files, self.limits.syntax());
        Ok(Self {
            root: self.root.clone(),
            files,
            text_files,
            left_out,
            composition: composition()?,
            limits: self.limits,
            language: Arc::clone(&self.language),
            content_cache: self.content_cache.clone(),
            symbol_documents,
            text_inclusion: self.text_inclusion.clone(),
            fingerprint,
            semantics,
            documentation: Arc::new(documentation),
            documentation_layer: OnceLock::new(),
            split_line_files: OnceLock::new(),
            notebooks,
            warnings,
        })
    }

    /// Reads one changed visible path into syntax facts and baseline content catalog.
    fn read_indexed_path(
        &self,
        path: &ProjectPath,
        contents: &mut IndexContents,
        workspace_bytes: &mut usize,
    ) -> Result<(), RiftError> {
        let absolute = self.root.join(path.as_str());
        if !absolute.is_file() {
            return Ok(());
        }
        let Some(class) = self.language.classifies(&absolute)? else {
            return Ok(());
        };
        let lockfile = self.language.excludes_lockfile(&absolute);
        match read_catalog_file(&self.root, &absolute, self.limits, workspace_bytes)? {
            IndexRead::Included(text_file) if lockfile => contents.hold_lockfile(&text_file),
            IndexRead::Included(text_file) => match class {
                ClassifiedPath::Source(provider) => {
                    contents.hold_source_file_with_cache(
                        text_file,
                        &absolute,
                        provider,
                        self.limits.syntax(),
                        &self.content_cache,
                        self.limits.files_max(),
                    )?;
                }
                ClassifiedPath::Text => contents.hold_text_file(text_file),
            },
            IndexRead::Skipped(warning) => contents.leave_out(warning),
        }
        Ok(())
    }

    /// Bytes the held contents already contribute to the workspace byte bound.
    fn indexed_bytes(
        files: &BTreeMap<ProjectPath, Arc<IndexedFile>>,
        text_files: &BTreeMap<ProjectPath, Arc<TextSourceFile>>,
        left_out: &BTreeMap<ProjectPath, LeftOutFileState>,
    ) -> usize {
        let catalog: usize = text_files.values().map(|file| file.content().len()).sum();
        let syntax_only: usize = files
            .iter()
            .filter(|(path, _)| !text_files.contains_key(*path))
            .map(|(_, file)| file.source().len())
            .sum();
        let left_out_bytes: usize = left_out
            .iter()
            .filter(|(path, _)| !files.contains_key(*path) && !text_files.contains_key(*path))
            .map(|(_, state)| state.bytes)
            .sum();
        catalog
            .saturating_add(syntax_only)
            .saturating_add(left_out_bytes)
    }

    /// Assembles an index from files another source already accepted - the
    /// revision build, whose bytes come from git objects instead of a
    /// directory walk.
    #[expect(
        clippy::too_many_arguments,
        reason = "Inputs define one index publication over already accepted files."
    )]
    pub(crate) fn from_parts(
        root: PathBuf,
        contents: IndexContents,
        composition: ProviderComposition,
        limits: WorkspaceIndexLimits,
        language: Arc<WorkspaceLanguagePolicy>,
        text_inclusion: TextFileInclusion,
        content_cache: WorkspaceContentCache,
        previous: Option<&Self>,
    ) -> Result<Self, RiftError> {
        let BuiltContents {
            files,
            text_files,
            left_out,
            warnings,
            fingerprint,
            semantics,
        } = built_contents(
            &root,
            contents.sorted(),
            limits.declarations_max(),
            previous.map(|index| index.semantics.graph()),
        )?;
        let declarations = crate::documentation::declarations(&files, &semantics);
        let (documentation, notebooks) = crate::documentation::build(
            &files,
            &text_files,
            &declarations,
            &documentation_selection(&text_inclusion)?,
            checked_chunk_bytes_max(text_inclusion.chunk_bytes_max()),
            previous.map(|index| (&*index.documentation, &index.notebooks)),
        )?;
        let symbol_documents = carried_symbol_documents(previous, &files, limits.syntax());
        Ok(Self {
            root,
            files,
            text_files,
            left_out,
            composition,
            limits,
            language,
            content_cache,
            symbol_documents,
            text_inclusion,
            fingerprint,
            semantics,
            documentation: Arc::new(documentation),
            documentation_layer: OnceLock::new(),
            split_line_files: OnceLock::new(),
            notebooks,
            warnings,
        })
    }

    /// Returns canonical real workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bounds this index was built under.
    #[must_use]
    pub const fn limits(&self) -> WorkspaceIndexLimits {
        self.limits
    }

    /// Effective language path policy used by this publication.
    #[must_use]
    pub fn language_policy(&self) -> &WorkspaceLanguagePolicy {
        &self.language
    }

    /// Returns the indexed source files in project-path order.
    pub fn files(&self) -> impl ExactSizeIterator<Item = &IndexedFile> {
        self.files.values().map(AsRef::as_ref)
    }

    /// Returns baseline text files in project-path order.
    pub fn text_files(&self) -> impl ExactSizeIterator<Item = &TextSourceFile> {
        self.text_files.values().map(AsRef::as_ref)
    }

    /// Returns the validated documentation metadata for this workspace snapshot.
    #[must_use]
    pub fn documentation(&self) -> &DocumentationCollection {
        &self.documentation
    }

    /// The documentation layer over this snapshot's collection.
    ///
    /// The first call builds the layer and every later call answers it, so the reads one
    /// publication serves share its mappings.
    ///
    /// # Errors
    ///
    /// Returns the refusal the build met, kept for every later call: a layer bound
    /// crossed.
    pub fn documentation_layer(&self) -> Result<&DocumentationLayer<'static>, &RiftError> {
        self.documentation_layer
            .get_or_init(|| {
                rift_tracing::traced!(
                    component = "documentation",
                    operation = "documentation.layer",
                    corpus = "project",
                    { DocumentationLayer::shared([Arc::clone(&self.documentation)]) }
                )
            })
            .as_ref()
    }

    /// Keeps this snapshot's documentation collection alive for one publication.
    #[must_use]
    pub fn documentation_snapshot(&self) -> Arc<DocumentationCollection> {
        Arc::clone(&self.documentation)
    }

    /// Returns bytes addressed by one documentation content owner.
    #[must_use]
    pub fn documentation_content(&self, identity: &DocumentationContentIdentity) -> Option<&str> {
        let DocumentationSourceIdentity::Project { path } = &identity.source else {
            return None;
        };
        let path = rift_core::ProjectPath::new(path.0.as_str()).ok()?;
        if identity.cell.is_some() {
            return crate::documentation::content(&self.notebooks, identity);
        }
        self.files
            .get(&path)
            .map(|file| file.source())
            .or_else(|| self.text_files.get(&path).map(|file| file.content()))
    }

    /// How many source files this index holds.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// How many baseline text files this index holds.
    #[must_use]
    pub fn text_file_count(&self) -> usize {
        self.text_files.len()
    }

    /// How many files this build left out: each past the per-file byte bound,
    /// not UTF-8, holding a NUL byte, refused by its syntax provider under
    /// one of its bounds, or holding a declaration the Contribution contract
    /// refuses. Bounded by the walk's own `files_max`, since every warning
    /// names one walked file.
    #[must_use]
    pub fn left_out_file_count(&self) -> usize {
        self.warnings.len()
    }

    /// Every file's digest this build read, held or left out, in project-path order.
    ///
    /// A request that captured the tree itself compares its capture with this to name the
    /// files that moved, so a file the build left out keeps its digest here: the capture
    /// reads it without parsing and would otherwise report it as new on every read.
    #[must_use]
    pub fn digests(&self) -> WorkspaceDigests {
        keyed_digests(&self.files, &self.text_files, &self.left_out)
    }

    /// Every file's content digest this build holds, the files it left out included, in
    /// project-path order.
    ///
    /// [`Self::digests`] hashes each file's bytes again into the file-state digest an
    /// observation compares; this reads the content digest the build already took, the
    /// one [`Self::digest`] answers per path. It is what the lexical store records beside
    /// a file's rows, and what a later build compares itself against before it writes.
    #[must_use]
    pub fn content_digests(&self) -> WorkspaceDigests {
        WorkspaceDigests::new(
            self.left_out
                .iter()
                .map(|(path, state)| (path.clone(), state.content))
                .chain(
                    self.text_files
                        .iter()
                        .map(|(path, file)| (path.clone(), file.digest())),
                )
                .chain(
                    self.files
                        .iter()
                        .map(|(path, file)| (path.clone(), file.digest())),
                ),
        )
    }

    /// The tree revision this build's syntax-indexed files fold to, at full SHA-256
    /// length: each file's path and content digest in project-path order. A request-time
    /// capture of the same tree folds to the same value.
    #[must_use]
    pub fn tree_revision(&self) -> String {
        tree_revision_of(self.files.iter().map(|(path, file)| (path, file.digest())))
    }

    /// Returns the digest of the bytes read at `path`, whichever class holds it, the
    /// files left out included.
    ///
    /// This is what resolves an observation into a change set: the caller hashes the
    /// path's current bytes and compares them with what this publication indexed.
    #[must_use]
    pub fn digest(&self, path: &ProjectPath) -> Option<FileDigest> {
        self.files
            .get(path)
            .map(|file| file.digest())
            .or_else(|| self.text_files.get(path).map(|file| file.digest()))
            .or_else(|| self.left_out.get(path).map(|state| state.content))
    }

    /// The files this index leaves out of search as lockfiles `[search.text]
    /// excluded_lockfiles` names, in project-path order. Each one's digests stay recorded,
    /// but no search answers from it.
    pub fn excluded_lockfiles(&self) -> impl Iterator<Item = &ProjectPath> {
        self.left_out
            .keys()
            .filter(|path| self.language.excludes_lockfile(Path::new(path.as_str())))
    }

    /// What this index records at `path`: the digest of the bytes it read there, the
    /// files it left out after reading them included, or the warning naming a file it left
    /// out before reading a digest.
    ///
    /// Warnings sit in project-path order, so the lookup is one binary search after the
    /// map probes [`Self::digest`] makes.
    #[must_use]
    pub fn record(&self, path: &ProjectPath) -> Option<FileRecord> {
        self.digest(path).map(FileRecord::Digest).or_else(|| {
            self.warnings
                .binary_search_by(|warning| warning.path().cmp(path))
                .ok()
                .map(|position| FileRecord::LeftOut(self.warnings[position].clone()))
        })
    }

    /// Whether this index holds at least one file below `directory`, the files it left
    /// out included.
    ///
    /// A filesystem event on a directory the index holds files under can move every one
    /// of them at once, and this is what tells such a directory apart from an
    /// extensionless file. Every held file sits in `text_files` and every other tree
    /// entry in `left_out`, so two ordered-map probes answer in logarithmic time.
    #[must_use]
    pub fn holds_files_below(&self, directory: &ProjectPath) -> bool {
        let prefix = format!("{}/", directory.as_str());
        holds_path_below(&self.text_files, &prefix) || holds_path_below(&self.left_out, &prefix)
    }

    /// Derives index documents from this index: one document per indexed symbol, carrying
    /// its name, qualified name, derived identifier terms, signature, and attached
    /// documentation, and one or more documents per file text search reads, parsed or text
    /// alone, carrying the file's text, the one copy the store keeps: one whole document
    /// when the file is within `[search.text].max_chunk`, one per chunk otherwise. A chunked
    /// file's documents share its real path and share an identity built from that path plus
    /// the chunk index, so a hit still maps back to the file it came from.
    ///
    /// A fact no provider published stays absent. Nothing substitutes a declaration's own
    /// text into the signature or documentation field, because a reader weighs those
    /// fields apart and would then be weighing the same bytes twice.
    ///
    /// `force_include` files stay outside this derivation: that on-demand walk's contract
    /// covers source units read for one request, not the persistent lexical index.
    #[must_use]
    pub fn index_documents(&self) -> Vec<IndexDocument> {
        rift_tracing::traced!(
            component = "index",
            operation = "index.lexical_units",
            files = self.files.len(),
            {
                let mut documents = Vec::with_capacity(self.files.len() + self.text_files.len());
                let mut left_out = LeftOut::default();
                for file in self.files() {
                    for symbol in file.syntax().symbols() {
                        documents.extend(symbol_document(file, symbol, &mut left_out));
                    }
                }
                for file in self.searched_text_files() {
                    if is_notebook_path(file.path()) {
                        continue;
                    }
                    push_text_documents(
                        &mut documents,
                        file,
                        self.text_chunk_bytes_max(),
                        &mut left_out,
                    );
                }
                documents.extend(crate::documentation::cell_documents(
                    &self.notebooks,
                    self.text_chunk_bytes_max_usize(),
                    &mut left_out,
                ));
                left_out.report();
                documents
            }
        )
    }

    /// Symbol documents grouped by project file for vector population.
    ///
    /// Groups share unchanged documents with the previous index. Text documents stay out:
    /// they carry no declaration for vector embedding.
    #[must_use]
    pub fn symbol_index_documents_by_file(&self) -> Vec<Arc<[IndexDocument]>> {
        let paths = self.files.keys().collect::<Vec<_>>();
        let (groups, left_out) = self.symbol_index_documents_for(&paths);
        left_out.report();
        groups
    }

    fn symbol_index_documents_for(
        &self,
        paths: &[&ProjectPath],
    ) -> (Vec<Arc<[IndexDocument]>>, LeftOut) {
        let mut cache = self
            .symbol_documents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut groups = Vec::with_capacity(paths.len());
        let mut left_out = LeftOut::default();
        for path in paths {
            let Some(file) = self.files.get(*path) else {
                continue;
            };
            let key = symbol_document_key(file, self.limits.syntax());
            if let Some(cached) = cache.get(*path).filter(|cached| cached.key == key) {
                groups.push(Arc::clone(&cached.documents));
                continue;
            }
            cache.remove(*path);
            let mut file_left_out = LeftOut::default();
            let documents = file
                .syntax()
                .symbols()
                .iter()
                .filter_map(|symbol| symbol_document(file, symbol, &mut file_left_out))
                .collect::<Vec<_>>();
            left_out.count += file_left_out.count;
            if left_out.first.is_none() {
                left_out.first.clone_from(&file_left_out.first);
            }
            if documents.is_empty() {
                continue;
            }
            let documents: Arc<[IndexDocument]> = Arc::from(documents);
            if file_left_out.count == 0 {
                cache.insert(
                    (*path).clone(),
                    CachedSymbolDocuments {
                        key,
                        documents: Arc::clone(&documents),
                    },
                );
            }
            groups.push(documents);
        }
        (groups, left_out)
    }

    /// Records this index's declarations against every identifier `query` carried.
    ///
    /// Each extracted candidate is matched separately and an identity keeps its best class,
    /// so a question naming two identifiers ranks a declaration by the stronger of the two
    /// rather than by whichever was written first. `included` is the caller's own path
    /// selector; a declaration it excludes contributes nothing.
    ///
    /// Work is bounded twice over: the query contributes at most a fixed number of
    /// candidates, and each of them answers at most `bound` declarations.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a matched declaration cannot be read.
    pub fn observe_identifiers(
        &self,
        query: &ParsedQuery,
        bound: usize,
        included: impl Fn(&IndexedFile) -> bool,
        ranking: &mut IdentifierRanking,
    ) -> Result<(), RiftError> {
        for candidate in query.candidates() {
            for matched in self.symbols(candidate.text(), bound)? {
                if !included(matched.file) {
                    continue;
                }
                ranking.observe(declaration_identity(matched), matched.rank, &candidate);
            }
        }
        Ok(())
    }

    /// This index's declarations as one identifier ranking input, best first.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a matched declaration cannot be read.
    pub fn identifier_input(
        &self,
        query: &ParsedQuery,
        bound: usize,
    ) -> Result<RankingInput, RiftError> {
        let mut ranking = IdentifierRanking::new();
        self.observe_identifiers(query, bound, |_| true, &mut ranking)?;
        Ok(ranking.into_input(bound))
    }

    /// Derives index documents for the named paths alone, in the same shapes
    /// [`Self::index_documents`] derives for the whole index.
    ///
    /// A path this index no longer holds contributes nothing, which is what a removed file
    /// owes: its stored documents are deleted by path rather than replaced.
    #[must_use]
    pub fn index_documents_for<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a ProjectPath>,
    ) -> Vec<IndexDocument> {
        let paths = paths.into_iter().cloned().collect::<BTreeSet<_>>();
        let mut units = Vec::new();
        let mut left_out = LeftOut::default();
        for path in &paths {
            if let Some(file) = self.files.get(path) {
                for symbol in file.syntax().symbols() {
                    units.extend(symbol_document(file, symbol, &mut left_out));
                }
            }
            if let Some(file) = self.text_files.get(path)
                && !is_notebook_path(file.path())
                && !self.text_inclusion.skips(file.content().len())
            {
                push_text_documents(&mut units, file, self.text_chunk_bytes_max(), &mut left_out);
            }
        }
        units.extend(crate::documentation::cell_documents_for(
            &self.notebooks,
            self.text_chunk_bytes_max_usize(),
            &paths,
            &mut left_out,
        ));
        units
    }

    /// Returns each text file split into more than one lexical chunk, paired with its chunk
    /// count, so a caller can report the split instead of the index silently absorbing it.
    #[must_use]
    pub fn chunked_text_files(&self) -> Vec<(ProjectPath, usize)> {
        self.searched_text_files()
            .filter(|file| exceeds_chunk_bound(file.content().len(), self.text_chunk_bytes_max()))
            .map(|file| {
                let chunks = text_chunks(
                    file.content(),
                    checked_chunk_bytes_max(self.text_chunk_bytes_max()),
                );
                (file.path().clone(), chunks.len())
            })
            .collect()
    }

    /// Returns typed provider recipe used for this index.
    #[must_use]
    pub const fn composition(&self) -> &ProviderComposition {
        &self.composition
    }

    /// Returns exact visible source identity captured by this index.
    #[must_use]
    pub const fn fingerprint(&self) -> &WorkspaceFingerprint {
        &self.fingerprint
    }

    /// Returns normalized Contribution graph captured by this index.
    #[must_use]
    pub const fn normalized_graph(&self) -> &NormalizedGraph {
        self.semantics.graph()
    }

    /// Returns the symbol reference adjacency built from this index's normalized graph.
    #[must_use]
    pub const fn relationships(&self) -> &RelationshipStore {
        self.semantics.relationships()
    }

    /// Assembles readable symbol through its normalized record.
    ///
    /// # Errors
    ///
    /// Returns `RiftError` when normalized Contributions do not
    /// supply required portable facts.
    pub fn assembled_symbol(&self, matched: SymbolMatch<'_>) -> Result<ReadableSymbol, RiftError> {
        let identity = symbol_identity(
            &matched.file.syntax().language().identity_segment(),
            matched.file.path().as_str(),
            &matched.symbol.qualified_name,
        );
        ReadableSymbol::assembled_by(&self.semantics, &identity).ok_or_else(|| {
            errors::index::workspace_provider()
                .source(ReadableSymbolMissing { identity })
                .error()
        })
    }

    /// Files the build left out of the index, whole or in part, in project-path order.
    ///
    /// Build and rebuild continue after invalid UTF-8, NUL bytes, a file beyond
    /// the configured per-file byte bound, or a syntax tree the provider refuses
    /// under one of its bounds. A file refused for its source size alone keeps its text
    /// ([`WorkspaceIndexWarning::holds_text`]).
    #[must_use]
    pub fn warnings(&self) -> &[WorkspaceIndexWarning] {
        &self.warnings
    }

    /// Returns the maximum result count accepted per query against this index.
    #[must_use]
    pub const fn results_max(&self) -> usize {
        self.limits.results_max()
    }

    /// Finds declarations by exact, prefix, or substring name.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when limit exceeds configured maximum.
    pub fn symbols(&self, query: &str, limit: usize) -> Result<Vec<SymbolMatch<'_>>, RiftError> {
        self.validate_result_limit(limit)?;
        Ok(symbol_matches(self.files(), query, limit))
    }

    /// Finds lexical source lines containing query.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when limit exceeds configured maximum.
    pub fn source_matches(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(&IndexedFile, usize, String)>, RiftError> {
        self.validate_result_limit(limit)?;
        Ok(source_line_matches(self.files(), query, limit))
    }

    /// Finds lexical content lines containing `query` across included `[search.text]` files -
    /// the same content-line search [`Self::source_matches`] runs over syntax-indexed files,
    /// reaching a text-lane file's bytes directly rather than only through the vector ranking.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when limit exceeds configured maximum.
    pub fn text_matches(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(&TextSourceFile, usize, String)>, RiftError> {
        self.validate_result_limit(limit)?;
        Ok(text_line_matches(self.text_files(), query, limit))
    }

    /// Returns file by canonical project path.
    #[must_use]
    pub fn file(&self, path: &ProjectPath) -> Option<&IndexedFile> {
        self.files.get(path).map(AsRef::as_ref)
    }

    /// Parses one selected source path and keeps only nodes covering `position` for this read.
    ///
    /// Returns `None` when no provider selects `path`, `Skipped` when a catalog or syntax
    /// bound leaves it out, and `Included` when parsing succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when reading, path validation, or syntax analysis
    /// fails for a reason that does not leave the file out of the index.
    pub fn parse_source_file_with_nodes(
        &self,
        path: &ProjectPath,
        position: u64,
    ) -> Result<Option<IndexRead<IndexedFileNodes>>, RiftError> {
        let absolute = self.root.join(path.as_str());
        let Some(ClassifiedPath::Source(provider)) = self.language.classifies(&absolute)? else {
            return Ok(None);
        };
        let mut workspace_bytes = 0;
        let text_file =
            match read_catalog_file(&self.root, &absolute, self.limits, &mut workspace_bytes)? {
                IndexRead::Included(file) => file,
                IndexRead::Skipped(warning) => return Ok(Some(IndexRead::Skipped(warning))),
            };
        let syntax = match analyze_source(
            path,
            text_file.content(),
            &absolute,
            provider,
            self.limits.syntax(),
        ) {
            Ok(syntax) => syntax,
            Err(error) => match left_out_file(error, path.clone()) {
                Ok(Some(warning)) if warning.holds_text() => {
                    return Ok(Some(IndexRead::held_unparsed(warning)));
                }
                Ok(Some(warning)) => return Ok(Some(IndexRead::left_out(warning))),
                Ok(None) => unreachable!("recognized file refusal always names a warning"),
                Err(error) => return error.fail(),
            },
        };
        let nodes = syntax.nodes_at(position).into_iter().cloned().collect();
        let file = IndexedFile::new(
            text_file.path().clone(),
            Arc::clone(&text_file.content),
            text_file.digest(),
            text_file.executable(),
            syntax,
        );
        Ok(Some(IndexRead::Included(IndexedFileNodes { file, nodes })))
    }

    /// Returns one baseline text file by canonical project path.
    #[must_use]
    pub fn text_file(&self, path: &ProjectPath) -> Option<&TextSourceFile> {
        self.text_files.get(path).map(AsRef::as_ref)
    }

    /// Every held file whose text search reads, parsed or text alone, in project-path
    /// order: under `[search.text] large_files = "skip"`, a file past `max_chunk` is left
    /// out.
    pub fn searched_text_files(&self) -> impl Iterator<Item = &TextSourceFile> {
        self.text_files()
            .filter(|file| !self.text_inclusion.skips(file.content().len()))
    }

    /// The held files text search leaves out under `[search.text] large_files = "skip"`,
    /// in project-path order.
    pub fn skipped_text_files(&self) -> impl Iterator<Item = &TextSourceFile> {
        self.text_files()
            .filter(|file| self.text_inclusion.skips(file.content().len()))
    }

    /// Every file `[search.text] large_files = "skip"` keeps out of text search: the held
    /// files past `max_chunk`, then the files past the per-file byte bound the build left
    /// out, which under `skip` is `[providers.syntax] max_file`. Under `split` there are
    /// none.
    pub fn skipped_paths(&self) -> impl Iterator<Item = &ProjectPath> {
        let skip = self.text_inclusion.large_files() == LargeFileStrategy::Skip;
        let past_file_bound = self
            .warnings
            .iter()
            .filter(move |warning| {
                skip && matches!(warning, WorkspaceIndexWarning::FileTooLarge { .. })
            })
            .map(WorkspaceIndexWarning::path);
        self.skipped_text_files()
            .map(TextSourceFile::path)
            .chain(past_file_bound)
    }

    /// The searched files a `pattern` search verifies whole whatever the trigram index
    /// selects, in project-path order: a notebook, whose rows hold its cells rather than
    /// its bytes, and a file holding a line longer than `[search.text] max_chunk`, which
    /// chunking cut mid-line so that no one row holds the whole line.
    ///
    /// The long lines are found once per index: only a file past the chunk bound can hold
    /// one, and each such file is read once, by the first search that asks.
    pub fn whole_file_candidates(&self) -> impl Iterator<Item = &TextSourceFile> {
        let split = self.split_line_files.get_or_init(|| {
            let chunk_bytes_max = self.text_chunk_bytes_max_usize();
            self.searched_text_files()
                .filter(|file| holds_line_past(file.content(), chunk_bytes_max))
                .map(|file| file.path().clone())
                .collect()
        });
        self.searched_text_files()
            .filter(move |file| is_notebook_path(file.path()) || split.contains(file.path()))
    }

    /// Chunk bound applied to baseline text when lexical units are derived.
    fn text_chunk_bytes_max(&self) -> u64 {
        self.text_inclusion.chunk_bytes_max()
    }

    fn text_chunk_bytes_max_usize(&self) -> usize {
        checked_chunk_bytes_max(self.text_chunk_bytes_max())
    }

    /// Parses one selected source path and returns syntax nodes covering byte position.
    ///
    /// The selected provider and syntax bounds belong to this index. `None` means no
    /// provider selected the path; `Skipped` carries a catalog or syntax warning; `Included`
    /// carries the parsed file and matching nodes.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the provider refuses captured source or path
    /// policy no longer identifies the held file.
    pub fn nodes(
        &self,
        path: &ProjectPath,
        position: u64,
    ) -> Result<Option<Vec<SyntaxNode>>, RiftError> {
        let Some(file) = self.file(path) else {
            return Ok(None);
        };
        let absolute = self.root.join(path.as_str());
        let Some(ClassifiedPath::Source(provider)) = self.language.classifies(&absolute)? else {
            return errors::index::workspace_syntax().path(&absolute).fail();
        };
        let syntax = analyze_source(
            path,
            file.source(),
            &absolute,
            provider,
            self.limits.syntax(),
        )?;
        Ok(Some(
            syntax.nodes_at(position).into_iter().cloned().collect(),
        ))
    }

    /// Walks the workspace on demand for `.rs` files matching `force_include`'s globs that are
    /// not already indexed, ignoring `[source]` policy and `.gitignore` - only the hard floor
    /// (`.git`, `.rift`, `target`, symlinks) stays unreachable. Each match is parsed with the
    /// same syntax provider and per-file byte bound as the index, and the walk stops as soon
    /// as it would exceed `files_max`.
    ///
    /// Work is bounded by the same directory-depth limit as the index and by `files_max`
    /// matches; a `files_max`-plus-one-th match refuses rather than truncating silently.
    ///
    /// This walk covers provider source only. Baseline text uses its own bounded force-include
    /// walk so provider parsing remains separate from file content.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an invalid glob, an unreadable path, invalid UTF-8
    /// source, a syntax failure, or a file exceeding this index's per-file or aggregate byte
    /// bound, or `files_max` matches.
    pub fn force_include_files(
        &self,
        force_include: &[String],
        files_max: usize,
    ) -> Result<Vec<IndexedFile>, RiftError> {
        if force_include.is_empty() {
            return Ok(Vec::new());
        }
        let matcher = PathMatcher::build(&self.root, force_include, &[])?;
        let mut extra_bytes = 0_usize;
        let mut files = Vec::new();
        let walker = source_walk(
            &self.root,
            self.limits.directory_depth_max,
            GitignorePolicy::Ignore,
        );
        for entry in walker {
            let entry = entry.map_err(|error| walk_error(&self.root, error))?;
            let file_type = entry.file_type();
            if file_type.is_some_and(|file_type| file_type.is_dir()) {
                if entry.depth() > self.limits.directory_depth_max {
                    return errors::index::workspace_too_deep()
                        .path(entry.path())
                        .field("source.directory_depth")
                        .observed(entry.depth() as u64)
                        .maximum(self.limits.directory_depth_max as u64)
                        .fail();
                }
                continue;
            }
            if !file_type.is_some_and(|file_type| file_type.is_file()) {
                continue;
            }
            let path = entry.path();
            if !matcher.includes(path) {
                continue;
            }
            let Some(ClassifiedPath::Source(provider)) = self.language.classifies(path)? else {
                continue;
            };
            let relative = path.strip_prefix(&self.root).map_err(|error| {
                errors::index::workspace_invalid_path()
                    .path(path)
                    .source(error)
                    .error()
            })?;
            let project_path = relative_path(relative)?;
            if self.file(&project_path).is_some() {
                continue;
            }
            if files.len() >= files_max {
                return errors::index::workspace_too_many_files()
                    .path(path)
                    .field(SOURCE_FILES_FIELD)
                    .observed(files.len().saturating_add(1) as u64)
                    .maximum(files_max as u64)
                    .fail();
            }
            files.push(read_file(
                &self.root,
                path,
                provider,
                self.limits,
                &mut extra_bytes,
            )?);
        }
        Ok(files)
    }

    /// Walks request-selected visible files into baseline content catalog. Past `files_max`
    /// the walk reads nothing more and counts the selector's remaining matches, so the
    /// refusal carries the whole match count and names the first path past the bound; that
    /// counting walk is bounded by the tree and `directory_depth_max`, the same bounds the
    /// reading walk runs under.
    fn force_include_text_files(
        &self,
        force_include: &[String],
        files_max: usize,
    ) -> Result<Vec<TextSourceFile>, RiftError> {
        if force_include.is_empty() {
            return Ok(Vec::new());
        }
        let matcher = PathMatcher::build(&self.root, force_include, &[])?;
        let mut extra_bytes = 0_usize;
        let mut files = Vec::new();
        let mut match_count = 0_usize;
        let mut first_excess: Option<PathBuf> = None;
        let walker = source_walk(
            &self.root,
            self.limits.directory_depth_max,
            GitignorePolicy::Ignore,
        );
        for entry in walker {
            let entry = entry.map_err(|error| walk_error(&self.root, error))?;
            let file_type = entry.file_type();
            if file_type.is_some_and(|file_type| file_type.is_dir()) {
                if entry.depth() > self.limits.directory_depth_max {
                    return errors::index::workspace_too_deep()
                        .path(entry.path())
                        .field("source.directory_depth")
                        .observed(entry.depth() as u64)
                        .maximum(self.limits.directory_depth_max as u64)
                        .fail();
                }
                continue;
            }
            if !file_type.is_some_and(|file_type| file_type.is_file()) {
                continue;
            }
            let path = entry.path();
            if !matcher.includes(path) {
                continue;
            }
            let project_path = project_path_below(&self.root, path)?;
            if self.text_file(&project_path).is_some() {
                continue;
            }
            match_count += 1;
            if files.len() >= files_max {
                first_excess.get_or_insert_with(|| path.to_path_buf());
                continue;
            }
            if let IndexRead::Included(file) =
                read_catalog_file(&self.root, path, self.limits, &mut extra_bytes)?
            {
                files.push(file);
            }
        }
        match first_excess {
            Some(path) => errors::index::workspace_too_many_files()
                .path(&path)
                .field(FORCE_INCLUDE_FIELD)
                .observed(match_count)
                .maximum(files_max)
                .fail(),
            None => Ok(files),
        }
    }

    /// Builds one request index over files selected by force include.
    ///
    /// Each selected path is read once into baseline content catalog. Paths with a syntax
    /// provider also receive syntax facts from those same bytes.
    ///
    /// # Errors
    ///
    /// Returns `RiftError` when selection, parsing, or bounds fail.
    pub fn force_include_index(
        &self,
        force_include: &[String],
        files_max: usize,
    ) -> Result<Self, RiftError> {
        let text_files = self.force_include_text_files(force_include, files_max)?;
        let mut files = Vec::new();
        for file in &text_files {
            let context_path = self.root.join(file.path().as_str());
            if let Some(ClassifiedPath::Source(provider)) =
                self.language.classifies(&context_path)?
                && let IndexRead::Included(indexed) =
                    syntax_read(file, &context_path, provider, self.limits.syntax())?
            {
                files.push(indexed);
            }
        }
        Self::from_parts(
            self.root.clone(),
            IndexContents {
                files: keyed_by_path(files, IndexedFile::path),
                text_files: keyed_by_path(text_files, TextSourceFile::path),
                ..IndexContents::default()
            },
            self.composition.clone(),
            self.limits,
            Arc::clone(&self.language),
            self.text_inclusion.clone(),
            WorkspaceContentCache::default(),
            None,
        )
    }

    fn validate_result_limit(&self, limit: usize) -> Result<(), RiftError> {
        if limit == 0 || limit > self.limits.results_max {
            return errors::index::workspace_result_limit()
                .field("results")
                .observed(limit as u64)
                .maximum(self.limits.results_max as u64)
                .fail();
        }
        Ok(())
    }
}

/// Declaration matches for `query` across `files`, ranked qualified-exact first, then
/// name-exact, name-prefix, and qualified-name substring. Shared so an on-demand file set
/// (search's `force_include`) scores identically to the index.
pub fn symbol_matches<'a>(
    files: impl IntoIterator<Item = &'a IndexedFile>,
    query: &str,
    limit: usize,
) -> Vec<SymbolMatch<'a>> {
    let query = query.to_lowercase();
    let mut matches = files
        .into_iter()
        .flat_map(|file| {
            file.syntax()
                .symbols()
                .iter()
                .map(move |symbol| (file, symbol))
        })
        .filter_map(|(file, symbol)| {
            Some(SymbolMatch {
                file,
                symbol,
                rank: symbol_rank(symbol, &query)?,
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|matched| (matched.rank, matched.symbol.qualified_name.as_str()));
    matches.truncate(limit);
    matches
}

/// Lexical source-line matches for `query` across `files`. Shared the same way as
/// [`symbol_matches`].
pub fn source_line_matches<'a>(
    files: impl IntoIterator<Item = &'a IndexedFile>,
    query: &str,
    limit: usize,
) -> Vec<(&'a IndexedFile, usize, String)> {
    line_matches(files, IndexedFile::source, query, limit)
}

/// Lexical content-line matches for `query` across included `[search.text]` files. Shared the
/// same kernel as [`source_line_matches`]: the two file classes carry their whole text under
/// different accessors, and the line scan itself does not care which.
pub fn text_line_matches<'a>(
    files: impl IntoIterator<Item = &'a TextSourceFile>,
    query: &str,
    limit: usize,
) -> Vec<(&'a TextSourceFile, usize, String)> {
    line_matches(files, TextSourceFile::content, query, limit)
}

/// Case-insensitive line scan behind [`source_line_matches`] and [`text_line_matches`]:
/// `content` reads whichever field a file class holds its whole text in, and the scan itself
/// is one representation shared by both classes and by an on-demand file set
/// (`force_include`).
fn line_matches<'a, File>(
    files: impl IntoIterator<Item = &'a File>,
    content: impl Fn(&'a File) -> &'a str,
    query: &str,
    limit: usize,
) -> Vec<(&'a File, usize, String)> {
    let query = query.to_lowercase();
    let mut matches = Vec::new();
    for file in files {
        for (line_index, line) in content(file).lines().enumerate() {
            if line.to_lowercase().contains(&query) {
                matches.push((file, line_index + 1, line.into()));
                if matches.len() == limit {
                    return matches;
                }
            }
        }
    }
    matches
}

fn positive_bound(bound: usize) -> Result<(), RiftError> {
    if bound == 0 {
        return errors::index::workspace_zero_limit().fail();
    }
    Ok(())
}

fn canonical_root(root: &Path) -> Result<PathBuf, RiftError> {
    let canonical = fs::canonicalize(root).map_err(|error| {
        errors::index::workspace_invalid_root()
            .path(root)
            .source(error)
            .error()
    })?;
    if !canonical.is_dir() {
        return errors::index::workspace_invalid_root()
            .path(&canonical)
            .fail();
    }
    Ok(canonical)
}

fn composition() -> Result<ProviderComposition, RiftError> {
    let source = component::<(), WorkspaceFiles>("workspace-source")?;
    let syntax = component::<WorkspaceFiles, RustFacts>("rust-tree-sitter")?;
    let index = component::<RustFacts, ReadIndex>("memory-index")?;
    let mut builder = CompositionBuilder::new(
        CompositionId::new("rust-read")
            .map_err(|source| errors::index::workspace_composition().cause(source).error())?,
    );
    let files = builder.source("project", &source);
    let facts = builder.then(files, "syntax", &syntax);
    let reads = builder.then(facts, "index", &index);
    builder
        .output(reads)
        .build()
        .map_err(|source| errors::index::workspace_composition().cause(source).error())
}

pub(crate) fn component<Input: 'static, Output: 'static>(
    id: &str,
) -> Result<Component<Input, Output>, RiftError> {
    Ok(Component::new(ProviderId::new(id).map_err(|source| {
        errors::index::workspace_composition().cause(source).error()
    })?))
}

/// The contents one build assembles its index from, with the semantics graph built
/// over them.
struct BuiltContents {
    files: BTreeMap<ProjectPath, Arc<IndexedFile>>,
    text_files: BTreeMap<ProjectPath, Arc<TextSourceFile>>,
    left_out: BTreeMap<ProjectPath, LeftOutFileState>,
    warnings: Vec<WorkspaceIndexWarning>,
    fingerprint: WorkspaceFingerprint,
    semantics: WorkspaceSemantics,
}

/// Builds the semantics graph over `contents`, leaving out every held file whose
/// declarations the Contribution contract refuses.
///
/// The graph is built over every held document at once. Refused Contributions name their
/// documents, so the build leaves all such files out together, keeps their digests, then
/// builds the graph again. A failure the left-out rule does not route fails the build with
/// the fault, path attached when known.
///
/// `declarations_max` is the publication's own capacity, the `[source]` table's
/// `declarations`. A workspace carrying more declarations than that publishes the files
/// that fit and leaves the rest out, so the index serves what it can instead of refusing
/// the whole build.
fn built_contents(
    root: &Path,
    mut contents: IndexContents,
    declarations_max: usize,
    previous: Option<&NormalizedGraph>,
) -> Result<BuiltContents, RiftError> {
    let passes_max = contents.files.len().saturating_add(1);
    let mut passes = 0_usize;
    loop {
        let fingerprint = WorkspaceFingerprint::from_files(
            &contents.files,
            &contents.text_files,
            &contents.left_out,
        );
        let built = WorkspaceSemantics::build_project_facts(
            contents
                .files
                .values()
                .map(|file| (file.syntax(), file.path())),
            declarations_max,
            fingerprint.revision_number(),
            previous,
        );
        match built {
            Ok(BuiltSemantics {
                semantics,
                beyond_declaration_bound,
                refused_contributions,
            }) => {
                if !beyond_declaration_bound.is_empty() || !refused_contributions.is_empty() {
                    passes = passes.saturating_add(1);
                    leave_out_refused_contributions(
                        &mut contents,
                        refused_contributions,
                        passes < passes_max,
                    )?;
                    leave_out_beyond_declaration_bound(
                        &mut contents,
                        &beyond_declaration_bound,
                        passes < passes_max,
                    )?;
                    continue;
                }
                let IndexContents {
                    files,
                    text_files,
                    left_out,
                    warnings,
                } = contents;
                return Ok(BuiltContents {
                    files,
                    text_files,
                    left_out,
                    warnings,
                    fingerprint,
                    semantics,
                });
            }
            Err(error) => {
                return error
                    .with(ctx::operation("index.build"))
                    .with(ctx::workspace(root))
                    .fail();
            }
        }
    }
}

/// Leaves every document whose Contribution the syntax publication refused out in one pass.
fn leave_out_refused_contributions(
    contents: &mut IndexContents,
    refused: Vec<(ProjectPath, RiftError)>,
    within_passes: bool,
) -> Result<(), RiftError> {
    for (path, error) in refused {
        let warning = WorkspaceIndexWarning::Contribution {
            path: path.clone(),
            error: Arc::new(error.with(ctx::operation("index.build"))),
        };
        if !within_passes || !contents.leave_out_held(&path, warning) {
            return errors::index::workspace_provider()
                .path(Path::new(path.as_str()))
                .fail();
        }
    }
    Ok(())
}

/// Leaves every document the declaration bound had no room for out of `contents`.
///
/// Each one keeps its digests, as every left-out file does, so the next capture of the
/// tree still compares against what this build read. One pass removes every named
/// document, so the pass after it publishes.
///
/// # Errors
///
/// Returns [`RiftError`] when the pass budget is spent or a named document is
/// not held, either of which means the removal cannot make progress.
fn leave_out_beyond_declaration_bound(
    contents: &mut IndexContents,
    beyond: &[ProjectPath],
    within_passes: bool,
) -> Result<(), RiftError> {
    for path in beyond {
        let warning = WorkspaceIndexWarning::DeclarationsBeyondBound(path.clone());
        if !within_passes || !contents.leave_out_held(path, warning) {
            return errors::index::workspace_provider()
                .path(Path::new(path.as_str()))
                .fail();
        }
    }
    Ok(())
}

/// Source, text, and lockfile paths [`discover`] found below one root, each list sorted
/// by path.
#[derive(Debug, Default)]
struct DiscoveredPaths {
    /// Each source path with the provider that claimed it, so the caller
    /// never asks the language table the same question twice.
    source: Vec<(PathBuf, &'static dyn SyntaxProvider)>,
    text: Vec<PathBuf>,
    /// The lockfiles `[search.text].excluded_lockfiles` leaves out of search: read for
    /// their digests alone, so the workspace still moves when one is edited.
    lockfiles: Vec<PathBuf>,
}

impl DiscoveredPaths {
    /// Records one classified path against the shared `files_max` budget.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the budget is already spent, naming the
    /// `[source]` key that owns it and the path that crossed it.
    fn admit(
        &mut self,
        files_max: usize,
        path: &Path,
        class: ClassifiedPath,
    ) -> Result<(), RiftError> {
        self.within_files_max(files_max, path)?;
        match class {
            ClassifiedPath::Source(provider) => self.source.push((path.to_path_buf(), provider)),
            ClassifiedPath::Text => self.text.push(path.to_path_buf()),
        }
        Ok(())
    }

    /// Records one lockfile search leaves out, against the same `files_max` budget.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the budget is already spent.
    fn admit_lockfile(&mut self, files_max: usize, path: &Path) -> Result<(), RiftError> {
        self.within_files_max(files_max, path)?;
        self.lockfiles.push(path.to_path_buf());
        Ok(())
    }

    /// Refuses one more path once `files_max` paths are recorded, naming the `[source]` key
    /// that owns the bound and the path that crossed it.
    fn within_files_max(&self, files_max: usize, path: &Path) -> Result<(), RiftError> {
        let total = self.source.len() + self.text.len() + self.lockfiles.len();
        if total >= files_max {
            return errors::index::workspace_too_many_files()
                .path(path)
                .field(SOURCE_FILES_FIELD)
                .observed(total.saturating_add(1))
                .maximum(files_max)
                .fail();
        }
        Ok(())
    }

    /// Records one classified path, a lockfile search leaves out among them.
    fn admit_classified(
        &mut self,
        files_max: usize,
        path: &Path,
        class: ClassifiedPath,
        language: &WorkspaceLanguagePolicy,
    ) -> Result<(), RiftError> {
        if language.excludes_lockfile(path) {
            self.admit_lockfile(files_max, path)
        } else {
            self.admit(files_max, path, class)
        }
    }
}

/// Source and baseline text paths visible below `root`: the hard floor (`.git`, `.rift`,
/// `target`, symlinks) is always applied, `visibility.respect_gitignore()` then layers the
/// workspace's own `.gitignore` chain, and the `[source]` globs decide in their own order -
/// `exclude` drops, `force_include` keeps what `.gitignore` hid, `include` narrows the rest.
/// A provider extension adds syntax facts; every other accepted file joins baseline text.
/// Both classes use the same `.gitignore` and `[source]` policy.
///
/// Directories are walked in file-name order so a bound violation is reported
/// deterministically; both returned lists are sorted by path. Source and text paths share one
/// `files_max` budget, counted as they are discovered, and [`discover_forced`] spends the same
/// budget on what the `.gitignore`-respecting walk could not reach.
fn discover_cancellable(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    language: &WorkspaceLanguagePolicy,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<DiscoveredPaths, RiftError> {
    let matcher = PathMatcher::build_with_force_include(
        root,
        visibility.include(),
        visibility.exclude(),
        visibility.force_include(),
    )?;
    let gitignore = GitignorePolicy::from_respecting(visibility.respect_gitignore());
    let mut discovered = DiscoveredPaths::default();
    for entry in source_walk(root, limits.directory_depth_max, gitignore) {
        check_cancelled(cancelled)?;
        let entry = entry.map_err(|error| walk_error(root, error))?;
        let Some(path) = walked_file(&entry, limits.directory_depth_max)? else {
            continue;
        };
        if !matcher.includes(path) {
            continue;
        }
        let Some(class) = language.classifies(path)? else {
            continue;
        };
        discovered.admit_classified(limits.files_max, path, class, language)?;
    }
    discover_forced(root, limits, &matcher, language, &mut discovered, cancelled)?;
    discovered
        .source
        .sort_by(|left, right| left.0.cmp(&right.0));
    discovered.text.sort();
    discovered.lockfiles.sort();
    Ok(discovered)
}

/// The paths `force_include` reaches that the `.gitignore`-respecting walk could not yield.
///
/// A workspace naming no `force_include` pattern runs nothing here. Otherwise the walk skips
/// the `.gitignore` chain, descends only the subtrees the patterns can reach, and admits a
/// path whose verdict is [`PathVerdict::ForceIncluded`]. It shares `files_max` with the first
/// walk, and a path the first walk already recorded is skipped rather than recorded twice.
fn discover_forced(
    root: &Path,
    limits: WorkspaceIndexLimits,
    matcher: &PathMatcher,
    language: &WorkspaceLanguagePolicy,
    discovered: &mut DiscoveredPaths,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(), RiftError> {
    let reach = matcher.force_include_reach();
    if reach.is_empty() {
        return Ok(());
    }
    let recorded: HashSet<PathBuf> = discovered
        .source
        .iter()
        .map(|(path, _)| path.clone())
        .chain(discovered.text.iter().cloned())
        .chain(discovered.lockfiles.iter().cloned())
        .collect();
    for entry in forced_walk(root, limits.directory_depth_max, reach) {
        check_cancelled(cancelled)?;
        let entry = entry.map_err(|error| walk_error(root, error))?;
        let Some(path) = walked_file(&entry, limits.directory_depth_max)? else {
            continue;
        };
        if matcher.verdict(path) != PathVerdict::ForceIncluded || recorded.contains(path) {
            continue;
        }
        let Some(class) = language.classifies(path)? else {
            continue;
        };
        discovered.admit_classified(limits.files_max, path, class, language)?;
    }
    Ok(())
}

fn check_cancelled(cancelled: &(dyn Fn() -> bool + Sync)) -> Result<(), RiftError> {
    if cancelled() {
        errors::index::workspace_cancelled().fail()
    } else {
        Ok(())
    }
}

/// The file one walked entry names: `None` for a directory within `directory_depth_max` and
/// for an entry that is neither file nor directory, and a [`errors::index::workspace_too_deep::SLUG`]
/// refusal for a directory past that bound. Both walks report the bound the same way.
fn walked_file(entry: &DirEntry, directory_depth_max: usize) -> Result<Option<&Path>, RiftError> {
    let file_type = entry.file_type();
    if file_type.is_some_and(|file_type| file_type.is_dir()) {
        if entry.depth() > directory_depth_max {
            return errors::index::workspace_too_deep()
                .path(entry.path())
                .field("source.directory_depth")
                .observed(entry.depth() as u64)
                .maximum(directory_depth_max as u64)
                .fail();
        }
        return Ok(None);
    }
    if !file_type.is_some_and(|file_type| file_type.is_file()) {
        return Ok(None);
    }
    Ok(Some(entry.path()))
}

/// Compiles bounded workspace `.gitignore` chain for direct event matching.
/// The workspace's `.gitignore` files, each compiled against the directory that declares
/// it, shallowest first.
///
/// Git reads every ignore file relative to its own directory, and a deeper file decides
/// over a shallower one. Compiling them all against the workspace root moves every pattern
/// up: a `.ruff_cache/.gitignore` holding `*` - which `ruff` writes into any workspace it
/// runs in - would then exclude every file in the workspace, and the watcher that consults
/// this policy would see no source event at all.
#[derive(Debug)]
pub(crate) struct GitignoreChain {
    layers: Vec<Gitignore>,
}

impl GitignoreChain {
    /// Compiles every `.gitignore` below `root`, each against its own directory.
    ///
    /// Work is bounded by [`WorkspaceIndexLimits::files_max`], counted over ignore files.
    fn build_cancellable(
        root: &Path,
        limits: WorkspaceIndexLimits,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let mut layers = Vec::new();
        let mut ignore_files = 0_usize;
        for entry in source_walk(root, limits.directory_depth_max, GitignorePolicy::Ignore) {
            check_cancelled(cancelled)?;
            let entry = entry.map_err(|error| walk_error(root, error))?;
            let path = entry.path();
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
                || path.file_name() != Some(OsStr::new(".gitignore"))
            {
                continue;
            }
            if ignore_files >= limits.files_max {
                return errors::index::workspace_too_many_files()
                    .path(path)
                    .field(SOURCE_FILES_FIELD)
                    .observed(ignore_files.saturating_add(1))
                    .maximum(limits.files_max)
                    .fail();
            }
            ignore_files += 1;
            layers.push(compiled_gitignore(path)?);
        }
        layers.sort_by_key(|layer| layer.path().components().count());
        Ok(Self { layers })
    }

    /// Whether the workspace's ignore files exclude `path`.
    ///
    /// Each layer whose directory contains `path` answers in turn, and the deepest one that
    /// matches decides, which is git's own precedence.
    fn excludes(&self, path: &Path, is_directory: bool) -> bool {
        let mut excluded = false;
        for layer in &self.layers {
            if !path.starts_with(layer.path()) {
                continue;
            }
            match layer.matched_path_or_any_parents(path, is_directory) {
                Match::Ignore(_) => excluded = true,
                Match::Whitelist(_) => excluded = false,
                Match::None => {}
            }
        }
        excluded
    }
}

/// Compiles one `.gitignore` file against the directory that declares it.
fn compiled_gitignore(path: &Path) -> Result<Gitignore, RiftError> {
    let directory = path.parent().unwrap_or(path);
    let mut builder = GitignoreBuilder::new(directory);
    if let Some(error) = builder.add(path) {
        return errors::index::workspace_filesystem()
            .path(path)
            .source(error)
            .fail();
    }
    builder.build().map_err(|error| {
        errors::index::workspace_filesystem()
            .path(path)
            .source(error)
            .error()
    })
}

/// Hashes one already-discovered source and text path set without parsing syntax. Both
/// classes enforce [`WorkspaceIndexLimits::file_bytes_max`] before counting accepted bytes
/// against `workspace_bytes_max`, matching the catalog read the index build applies. A
/// path whose stat matches `last` keeps its recorded digests without a read, and the
/// returned [`LastCapture`] records this capture's paths for the next one.
#[cfg(test)]
fn capture_paths(
    root: &Path,
    paths: &DiscoveredPaths,
    limits: WorkspaceIndexLimits,
    last: &LastCapture,
) -> Result<(WorkspaceDigests, LastCapture), RiftError> {
    capture_paths_with_boundary(
        root,
        paths,
        limits,
        last,
        CaptureBoundary::create(root),
        root_identity(root),
        &|| false,
    )
}

fn capture_paths_with_boundary(
    root: &Path,
    paths: &DiscoveredPaths,
    limits: WorkspaceIndexLimits,
    last: &LastCapture,
    boundary: Option<CaptureBoundary>,
    root_identity: Option<(u64, u64)>,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(WorkspaceDigests, LastCapture), RiftError> {
    let source: Vec<PathBuf> = paths.source.iter().map(|(path, _)| path.clone()).collect();
    let other: Vec<PathBuf> = paths.text.iter().chain(&paths.lockfiles).cloned().collect();
    capture_path_lists_with_boundary(
        root,
        limits,
        &CapturePathLists {
            source: &source,
            other: &other,
            last,
            boundary,
            root_identity,
            cancelled,
        },
    )
}

struct CapturePathLists<'a> {
    source: &'a [PathBuf],
    other: &'a [PathBuf],
    last: &'a LastCapture,
    boundary: Option<CaptureBoundary>,
    root_identity: Option<(u64, u64)>,
    cancelled: &'a (dyn Fn() -> bool + Sync),
}

fn capture_path_lists_with_boundary(
    root: &Path,
    limits: WorkspaceIndexLimits,
    paths: &CapturePathLists<'_>,
) -> Result<(WorkspaceDigests, LastCapture), RiftError> {
    let CapturePathLists {
        source,
        other,
        last,
        boundary,
        root_identity,
        cancelled,
    } = *paths;
    let mut workspace_bytes = 0_usize;
    let held = source
        .len()
        .checked_add(other.len())
        .ok_or_else(|| errors::index::workspace_too_many_files().path(root).error())?;
    let mut next = LastCapture::under(limits, held, boundary, root_identity);
    let mut capture = PathCapture {
        workspace_bytes: &mut workspace_bytes,
        root,
        limits,
        last,
        next: &mut next,
        boundary,
        root_identity,
        cancelled,
    };
    let (source, unparsed): (Vec<_>, Vec<_>) = capture
        .path_class(source)?
        .into_iter()
        .partition(|captured| captured.length <= limits.syntax().source_bytes_max());
    let text = capture.path_class(other)?;
    rift_tracing::debug!(
        component = "index",
        operation = "fingerprint.bytes",
        bytes = workspace_bytes,
        source = source.len(),
        text = text.len(),
    );
    let digests = WorkspaceDigests::classified(
        source
            .into_iter()
            .map(|captured| (captured.path, captured.state, captured.content)),
        text.into_iter()
            .chain(unparsed)
            .map(|captured| (captured.path, captured.state)),
    );
    Ok((digests, next))
}

/// One file the capture kept: its digests, and the bytes it counts. A claimed file past
/// the syntax source bound is held as text the provider does not parse, so the capture
/// files it beside the text files, as the build does.
struct CapturedEntry {
    path: ProjectPath,
    state: FileDigest,
    content: FileDigest,
    length: usize,
}

/// Reads every visible file's digest below `root`, without parsing syntax.
///
/// Work is bounded by [`WorkspaceIndexLimits`], the same bounds the index applies. A
/// claimed file whose bytes are not UTF-8 is omitted from the returned digests rather than
/// failing the capture, matching what [`WorkspaceIndex::build`] omits from the index over
/// the same tree.
///
/// # Errors
///
/// Returns [`RiftError`] for discovery, read, or configured-bound failures.
pub fn capture_digests(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
) -> Result<WorkspaceDigests, RiftError> {
    capture_digests_with_languages(
        root,
        limits,
        visibility,
        &TextFileInclusion::default(),
        &LanguageFileSelections::default(),
        &LastCapture::default(),
    )
    .map(|(digests, _)| digests)
}

/// Reads one effective language and text selection's digests below `root`, reusing the
/// digests `last` recorded for every path whose stat did not move. The returned
/// [`LastCapture`] records this capture's paths for the next one.
///
/// # Errors
///
/// Returns [`RiftError`] for configuration, discovery, read, or
/// configured-bound failures.
pub fn capture_digests_with_languages(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    text_inclusion: &TextFileInclusion,
    languages: &LanguageFileSelections,
    last: &LastCapture,
) -> Result<(WorkspaceDigests, LastCapture), RiftError> {
    capture_digests_with_languages_cancellable(
        root,
        limits,
        visibility,
        text_inclusion,
        languages,
        last,
        &|| false,
    )
}

/// Captures one effective language and text selection, checking `cancelled` between files.
///
/// # Errors
///
/// Returns [`RiftError`] for configuration, discovery, read, configured-bound,
/// or cancellation failures.
///
/// # Panics
///
/// Panics if all three attempts finish without a `ChangedDuringCapture` error. Each attempt
/// that continues the retry loop records that error.
pub fn capture_digests_with_languages_cancellable(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    text_inclusion: &TextFileInclusion,
    languages: &LanguageFileSelections,
    last: &LastCapture,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(WorkspaceDigests, LastCapture), RiftError> {
    check_cancelled(cancelled)?;
    let mut changed = None;
    for _ in 0..3 {
        match capture_digests_attempt(
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
            last,
            cancelled,
        ) {
            Err(error) if error.slug() == errors::index::workspace_changed_during_capture::SLUG => {
                changed = Some(error);
            }
            result => return result,
        }
    }
    changed
        .expect("three changed captures leave one change error")
        .fail()
}

/// Captures indexed file states and all visible content with the same stat record.
/// Selected source paths are read once and count against the indexed workspace-byte
/// bound. Other visible paths use per-file, file-count, cancellation, and
/// changed-during-capture checks, matching the visible catalog's admission.
///
/// # Errors
///
/// Returns [`RiftError`] for configuration, discovery, read, configured-bound,
/// or cancellation failures.
///
/// # Panics
///
/// Panics if all three attempts finish without a `ChangedDuringCapture` error. Each attempt
/// that continues the retry loop records that error.
pub fn capture_visible_digests_with_languages_cancellable(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    text_inclusion: &TextFileInclusion,
    languages: &LanguageFileSelections,
    last: &LastCapture,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(WorkspaceDigests, WorkspaceDigests, LastCapture), RiftError> {
    let mut changed = None;
    for _ in 0..3 {
        let result: Result<_, RiftError> = (|| {
            check_cancelled(cancelled)?;
            let root = canonical_root(root)?;
            let (indexed, mut next) = capture_digests_attempt(
                &root,
                limits,
                visibility,
                text_inclusion,
                languages,
                last,
                cancelled,
            )?;
            let policy = WorkspaceSourcePolicy::build_with_languages_cancellable(
                &root,
                limits,
                visibility,
                text_inclusion,
                languages,
                cancelled,
            )?;
            let paths = policy.visible_paths_cancellable(cancelled)?;
            let boundary = next.boundary();
            let root_identity = next.root_identity();
            let mut visible = Vec::with_capacity(paths.len());
            for batch in paths.chunks(SOURCE_BATCH_FILES) {
                check_cancelled(cancelled)?;
                let read: Vec<Result<(PathBuf, CapturedPath, bool), RiftError>> = batch
                    .par_iter()
                    .map(|path| {
                        check_cancelled(cancelled)?;
                        let absolute = root.join(path.as_str());
                        let (capture, was_read) = match next.captured(&absolute) {
                            Some(capture) => (capture, false),
                            None => capture_path(&absolute, limits, last, boundary, root_identity)?,
                        };
                        Ok((absolute, capture, was_read))
                    })
                    .collect();
                for (path, capture) in batch.iter().zip(read) {
                    check_cancelled(cancelled)?;
                    let (absolute, capture, was_read) = capture?;
                    if next.captured(&absolute).is_none() {
                        next.record(&absolute, capture, was_read);
                    }
                    if let Some((_, digest)) = capture.content() {
                        visible.push((path.clone(), digest));
                    }
                }
            }
            Ok((indexed, WorkspaceDigests::new(visible), next))
        })();
        match result {
            Err(error) if error.slug() == errors::index::workspace_changed_during_capture::SLUG => {
                changed = Some(error);
            }
            result => return result,
        }
    }
    changed
        .expect("three changed captures leave one change error")
        .fail()
}

/// Captures one publication's selected source and other paths without walking the workspace.
///
/// # Errors
///
/// Returns [`RiftError`] for an invalid root, duplicate or over-bound paths, read,
/// configured-bound, or cancellation failures.
pub fn capture_selected_paths_cancellable(
    root: &Path,
    limits: WorkspaceIndexLimits,
    source: &[ProjectPath],
    other: &[ProjectPath],
    last: &LastCapture,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(WorkspaceDigests, LastCapture), RiftError> {
    let root = canonical_root(root)?;
    let total = source.len().checked_add(other.len()).ok_or_else(|| {
        errors::index::workspace_too_many_files()
            .path(&root)
            .error()
    })?;
    if total > limits.files_max() {
        return errors::index::workspace_too_many_files()
            .path(&root)
            .field(SOURCE_FILES_FIELD)
            .observed(total)
            .maximum(limits.files_max())
            .fail();
    }
    let unique = source.iter().chain(other).collect::<BTreeSet<_>>().len();
    if unique != total {
        return errors::index::workspace_invalid_path().fail();
    }
    let source = source
        .iter()
        .map(|path| root.join(path.as_str()))
        .collect::<Vec<_>>();
    let other = other
        .iter()
        .map(|path| root.join(path.as_str()))
        .collect::<Vec<_>>();
    let boundary = CaptureBoundary::create(&root);
    let root_identity = root_identity(&root);
    capture_path_lists_with_boundary(
        &root,
        limits,
        &CapturePathLists {
            source: &source,
            other: &other,
            last,
            boundary,
            root_identity,
            cancelled,
        },
    )
}

fn capture_digests_attempt(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    text_inclusion: &TextFileInclusion,
    languages: &LanguageFileSelections,
    last: &LastCapture,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(WorkspaceDigests, LastCapture), RiftError> {
    let root = canonical_root(root)?;
    let boundary = CaptureBoundary::create(&root);
    let root_identity = root_identity(&root);
    let language = rift_tracing::traced!(
        component = "index",
        operation = "fingerprint.language_policy",
        { WorkspaceLanguagePolicy::build(&root, languages, text_inclusion) }
    )?;
    let classified =
        rift_tracing::traced!(component = "index", operation = "fingerprint.discover", {
            discover_cancellable(&root, limits, visibility, &language, cancelled)
        })?;
    let source = classified.source.len();
    let text = classified.text.len() + classified.lockfiles.len();
    rift_tracing::traced!(
        component = "index",
        operation = "fingerprint.read",
        source = source,
        text = text,
        {
            capture_paths_with_boundary(
                &root,
                &classified,
                limits,
                last,
                boundary,
                root_identity,
                cancelled,
            )
        }
    )
}

/// One request-time capture's running state: the bytes kept so far, and the record the
/// next capture reuses.
struct PathCapture<'capture> {
    workspace_bytes: &'capture mut usize,
    root: &'capture Path,
    limits: WorkspaceIndexLimits,
    last: &'capture LastCapture,
    next: &'capture mut LastCapture,
    boundary: Option<CaptureBoundary>,
    root_identity: Option<(u64, u64)>,
    cancelled: &'capture (dyn Fn() -> bool + Sync),
}

impl PathCapture<'_> {
    /// Reads one path class into captured file states: each kept file's project path, its
    /// file-state digest, and its content digest, in walk order. Each path is recorded in
    /// `next`, with its stat and what capturing it found.
    ///
    /// A path whose stat matches `last` keeps its recorded digests. Every other file is
    /// read and hashed across the rayon pool, and no file's bytes outlive its own digest,
    /// so the capture holds one file per worker. The kept lengths, reused ones included,
    /// are then summed in walk order: the `workspace_bytes_max` refusal names the path a
    /// sequential read would name, and an earlier path's failure wins over a later one's.
    fn path_class(&mut self, paths: &[PathBuf]) -> Result<Vec<CapturedEntry>, RiftError> {
        check_cancelled(self.cancelled)?;
        let (limits, last, boundary, root_identity, cancelled) = (
            self.limits,
            self.last,
            self.boundary,
            self.root_identity,
            self.cancelled,
        );
        let read: Vec<Result<(CapturedPath, bool), RiftError>> = paths
            .par_iter()
            .map(|path| {
                check_cancelled(cancelled)?;
                capture_path(path, limits, last, boundary, root_identity)
            })
            .collect();
        let mut captured = Vec::with_capacity(paths.len());
        for (path, capture) in paths.iter().zip(read) {
            check_cancelled(cancelled)?;
            let (capture, was_read) = capture?;
            self.next.record(path, capture, was_read);
            let Some(file) = capture.file() else {
                continue;
            };
            count_workspace_bytes(self.workspace_bytes, file.length, path, limits)?;
            captured.push(CapturedEntry {
                path: project_path_below(self.root, path)?,
                state: file.state,
                content: file.content,
                length: file.length,
            });
        }
        Ok(captured)
    }
}

impl WorkspaceDigests {
    /// The workspace identity these digests fold to.
    #[must_use]
    pub fn fingerprint(&self) -> WorkspaceFingerprint {
        WorkspaceFingerprint::from_digests(self)
    }
}

/// One file-state set over both file classes and the files left out, keyed by project path.
fn keyed_digests(
    files: &BTreeMap<ProjectPath, Arc<IndexedFile>>,
    text_files: &BTreeMap<ProjectPath, Arc<TextSourceFile>>,
    left_out: &BTreeMap<ProjectPath, LeftOutFileState>,
) -> WorkspaceDigests {
    WorkspaceDigests::classified(
        files.iter().map(|(path, file)| {
            (
                path.clone(),
                FileDigest::of_file_state(file.source().as_bytes(), file.executable()),
                file.digest(),
            )
        }),
        text_files
            .iter()
            .map(|(path, file)| {
                (
                    path.clone(),
                    FileDigest::of_file_state(file.content().as_bytes(), file.executable()),
                )
            })
            .chain(
                left_out
                    .iter()
                    .map(|(path, state)| (path.clone(), state.state)),
            ),
    )
}

/// Whether a map keyed by project path holds a key below `prefix`, one directory's
/// spelling with its trailing separator: the first key at or after the prefix in path
/// order lies below that directory exactly when it starts with the prefix.
fn holds_path_below<Value>(keyed: &BTreeMap<ProjectPath, Value>, prefix: &str) -> bool {
    keyed
        .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
        .next()
        .is_some_and(|(path, _)| path.as_str().starts_with(prefix))
}

/// Keys an accepted file list by project path, sharing each file behind one `Arc`.
///
/// Two entries spelling one path cannot both be indexed: the later one wins, which is the
/// order a directory walk would have left behind anyway.
fn keyed_by_path<File>(
    files: Vec<File>,
    path_of: impl Fn(&File) -> &ProjectPath,
) -> BTreeMap<ProjectPath, Arc<File>> {
    files
        .into_iter()
        .map(|file| (path_of(&file).clone(), Arc::new(file)))
        .collect()
}

/// Adds one unambiguous project-path and content-digest pair to workspace identity.
fn update_fingerprint(hasher: &mut Sha256, path: &ProjectPath, digest: FileDigest) {
    hasher.update(path.as_str().as_bytes());
    hasher.update([FINGERPRINT_PATH_SEPARATOR]);
    hasher.update(digest.as_bytes());
    hasher.update([FINGERPRINT_FILE_SEPARATOR]);
}

/// Whether a workspace walk also consults the workspace's own `.gitignore` chain, on top of the
/// hard floor it always applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitignorePolicy {
    /// `.gitignore` files, root and nested, hide the paths they match.
    Respect,
    /// `.gitignore` is not consulted; only the hard floor stays unreachable.
    Ignore,
}

impl GitignorePolicy {
    /// The policy matching `SourceVisibility::respect_gitignore`'s configured value.
    const fn from_respecting(respect_gitignore: bool) -> Self {
        if respect_gitignore {
            Self::Respect
        } else {
            Self::Ignore
        }
    }
}

/// One depth-bounded, hard-floor-filtered walk rooted at `root`, shared by the `[source]`-scoped
/// scan and `force_include`'s on-demand walk. `gitignore` selects whether the workspace's own
/// `.gitignore` chain also applies; the hard floor, depth bound, and file-name order are the
/// same either way.
fn source_walk(root: &Path, directory_depth_max: usize, gitignore: GitignorePolicy) -> Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(directory_depth_max.saturating_add(1)))
        .sort_by_file_name(OsStr::cmp)
        .filter_entry(hard_floor_includes)
        .git_ignore(gitignore == GitignorePolicy::Respect);
    builder.build()
}

/// One walk over the subtrees `force_include` reaches, with `.gitignore` not consulted. The
/// hard floor, depth bound, and file-name order are [`source_walk`]'s; `reach` prunes every
/// directory the patterns cannot lead to, so a workspace's ignored build output is never
/// descended for a pattern that names one other directory.
fn forced_walk(root: &Path, directory_depth_max: usize, reach: ForceIncludeReach) -> Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(directory_depth_max.saturating_add(1)))
        .sort_by_file_name(OsStr::cmp)
        .filter_entry(move |entry| hard_floor_includes(entry) && reach.reaches(entry.path()))
        .git_ignore(false);
    builder.build()
}

/// The hard floor every workspace applies before `.gitignore` or `[source]` are consulted:
/// `.git`, `.rift`, and `target` are never descended into or indexed - whether the name
/// resolves to a directory (the ordinary case, pruning the whole subtree) or, unusually, a
/// file (a `.git` file in a worktree checkout, a stray `.rift` marker) - and a symlink is
/// never followed or indexed. Excluding a file by this same name matters once an
/// extensionless candidate can join the text lane on its own: `ProjectPath` refuses any path
/// starting with `.rift`, so a `.rift` file left unfiltered here would abort the whole build
/// instead of simply staying invisible.
fn hard_floor_includes(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if entry.path_is_symlink() {
        return false;
    }
    let allowed_name = !is_hard_floor_name(entry.file_name());
    let linked_worktree = allowed_name
        && entry.file_type().is_some_and(|kind| kind.is_dir())
        && rift_history::Repository::is_linked_worktree(entry.path());
    allowed_name && !linked_worktree
}

/// Applies the hard floor to one absolute event path.
fn hard_floor_includes_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let allowed_names = !relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| WORKSPACE_IGNORED_DIRECTORIES.contains(&name))
    });
    let linked_worktree = allowed_names
        && path
            .ancestors()
            .take_while(|ancestor| *ancestor != root)
            .any(rift_history::Repository::is_linked_worktree);
    allowed_names && !linked_worktree
}

/// Whether `name` is one of the hard floor's names (`.git`, `.rift`, `target`), whatever the
/// entry it names turns out to be - a directory or, unusually, a file.
fn is_hard_floor_name(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| WORKSPACE_IGNORED_DIRECTORIES.contains(&name))
}

/// The path one `ignore` walk failure names, when its cause names one.
fn walk_source_path(error: &ignore::Error) -> Option<PathBuf> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path.clone()),
        ignore::Error::WithLineNumber { err, .. } | ignore::Error::WithDepth { err, .. } => {
            walk_source_path(err)
        }
        ignore::Error::Loop { child, .. } => Some(child.clone()),
        _ => None,
    }
}

fn walk_error(root: &Path, error: ignore::Error) -> RiftError {
    let path = walk_source_path(&error).unwrap_or_else(|| root.to_path_buf());
    errors::index::workspace_filesystem()
        .path(&path)
        .source(error)
        .error()
}

fn read_file(
    root: &Path,
    path: &Path,
    provider: &dyn SyntaxProvider,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<IndexedFile, RiftError> {
    let handle = fs::File::open(path).map_err(|error| {
        errors::index::workspace_filesystem()
            .path(path)
            .source(error)
            .error()
    })?;
    let metadata = handle.metadata().map_err(|error| {
        errors::index::workspace_filesystem()
            .path(path)
            .source(error)
            .error()
    })?;
    let bytes = read_file_bytes(handle, path, limits)?;
    let project_path = project_path_below(root, path)?;
    let mut file = included_file(project_path, bytes, path, provider, limits, workspace_bytes)?;
    file.set_executable(metadata_is_executable(&metadata));
    Ok(file)
}

/// Reads one cataloged file's syntax facts. A file the provider refuses for its source
/// size alone is held as text it does not parse; one refused under any other bound is
/// left out.
fn syntax_read(
    file: &TextSourceFile,
    context_path: &Path,
    provider: &dyn SyntaxProvider,
    limits: SyntaxLimits,
) -> Result<IndexRead<IndexedFile>, RiftError> {
    match indexed_file_from_catalog(file, context_path, provider, limits) {
        Ok(indexed) => Ok(IndexRead::Included(indexed)),
        Err(error) => match left_out_file(error, file.path().clone()) {
            Ok(Some(warning)) if warning.holds_text() => Ok(IndexRead::held_unparsed(warning)),
            Ok(Some(warning)) => Ok(IndexRead::left_out(warning)),
            Ok(None) => unreachable!("recognized file refusal always names a warning"),
            Err(error) => error.fail(),
        },
    }
}

pub(crate) fn indexed_file_from_catalog(
    file: &TextSourceFile,
    context_path: &Path,
    provider: &dyn SyntaxProvider,
    limits: SyntaxLimits,
) -> Result<IndexedFile, RiftError> {
    let syntax = analyze_source(file.path(), file.content(), context_path, provider, limits)?;
    Ok(IndexedFile::new(
        file.path().clone(),
        Arc::clone(&file.content),
        file.digest(),
        file.executable(),
        syntax,
    ))
}

fn analyze_source(
    path: &ProjectPath,
    source: &str,
    context_path: &Path,
    provider: &dyn SyntaxProvider,
    limits: SyntaxLimits,
) -> Result<rift_syntax::SyntaxDocument, RiftError> {
    provider
        .analyze(SyntaxSource { path, text: source }, limits)
        .map_err(|error| {
            errors::index::workspace_syntax()
                .path(context_path)
                .cause(error)
                .error()
        })
}

/// Includes one provider-backed file under the workspace bounds.
pub(crate) fn included_file(
    project_path: ProjectPath,
    bytes: Vec<u8>,
    context_path: &Path,
    provider: &dyn SyntaxProvider,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<IndexedFile, RiftError> {
    if bytes.len() > limits.file_bytes_max {
        return errors::index::workspace_file_too_large()
            .path(context_path)
            .field("source.file_bytes")
            .observed(bytes.len() as u64)
            .maximum(limits.file_bytes_max as u64)
            .fail();
    }
    *workspace_bytes = workspace_bytes.checked_add(bytes.len()).ok_or_else(|| {
        errors::index::workspace_workspace_too_large()
            .path(context_path)
            .field(SOURCE_WORKSPACE_SIZE_FIELD)
            .observed(u64::MAX)
            .maximum(limits.workspace_bytes_max as u64)
            .error()
    })?;
    if *workspace_bytes > limits.workspace_bytes_max {
        return errors::index::workspace_workspace_too_large()
            .path(context_path)
            .field(SOURCE_WORKSPACE_SIZE_FIELD)
            .observed(*workspace_bytes)
            .maximum(limits.workspace_bytes_max)
            .fail();
    }
    let source = source_utf8(bytes, context_path)?;
    let syntax = provider
        .analyze(
            SyntaxSource {
                path: &project_path,
                text: &source,
            },
            limits.syntax(),
        )
        .map_err(|error| {
            errors::index::workspace_syntax()
                .path(context_path)
                .cause(error)
                .error()
        })?;
    let digest = FileDigest::of(source.as_bytes());
    Ok(IndexedFile::new(
        project_path,
        Arc::new(source),
        digest,
        false,
        syntax,
    ))
}

#[cfg(test)]
fn read_text_file(
    root: &Path,
    path: &Path,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<TextSourceFile, RiftError> {
    let bytes = fs::read(path).map_err(|error| {
        errors::index::workspace_filesystem()
            .path(path)
            .source(error)
            .error()
    })?;
    let metadata = fs::metadata(path).map_err(|error| {
        errors::index::workspace_filesystem()
            .path(path)
            .source(error)
            .error()
    })?;
    let project_path = project_path_below(root, path)?;
    let mut file = included_text_file(project_path, bytes, path, limits, workspace_bytes)?;
    file.executable = metadata_is_executable(&metadata);
    Ok(file)
}

/// Source or text paths one parallel batch of the index build reads.
///
/// A batch is the unit the build reads ahead of its in-order fold, so it bounds how far
/// the reads run past a `workspace_bytes_max` refusal.
const SOURCE_BATCH_FILES: usize = 512;

/// Counts one kept file's bytes against `workspace_bytes_max`.
fn count_workspace_bytes(
    workspace_bytes: &mut usize,
    length: usize,
    context_path: &Path,
    limits: WorkspaceIndexLimits,
) -> Result<(), RiftError> {
    let total = workspace_bytes.checked_add(length).ok_or_else(|| {
        errors::index::workspace_workspace_too_large()
            .path(context_path)
            .field(SOURCE_WORKSPACE_SIZE_FIELD)
            .observed(u64::MAX)
            .maximum(limits.workspace_bytes_max() as u64)
            .error()
    })?;
    if total > limits.workspace_bytes_max() {
        return errors::index::workspace_workspace_too_large()
            .path(context_path)
            .field(SOURCE_WORKSPACE_SIZE_FIELD)
            .observed(total)
            .maximum(limits.workspace_bytes_max())
            .fail();
    }
    *workspace_bytes = total;
    Ok(())
}

/// One source file read on a pool worker, with its parse outcome kept for the in-order
/// fold: a parse failure is reported only after the file's bytes were counted.
struct ParsedSource {
    text_file: TextSourceFile,
    parsed: Result<IndexRead<Arc<IndexedFile>>, RiftError>,
}

impl ParsedSource {
    /// Reads one source file and parses it with the provider that claimed it, or takes the
    /// file `previous` parsed from the same bytes at the same path.
    fn read(
        root: &Path,
        path: &Path,
        provider: &dyn SyntaxProvider,
        limits: WorkspaceIndexLimits,
        previous: Option<&WorkspaceIndex>,
        content_cache: &WorkspaceContentCache,
    ) -> Result<IndexRead<Self>, RiftError> {
        Ok(match catalog_file(root, path, limits)? {
            IndexRead::Included(mut text_file) => {
                let digest = text_file.digest();
                let syntax_limits = limits.syntax();
                let (cached_source, cached_syntax) =
                    content_cache.get(digest, syntax_limits, provider);
                if let Some(source) = cached_source {
                    text_file.content = source;
                }
                let parsed = match previous.and_then(|index| index.parsed_as(&text_file)) {
                    Some(shared) => {
                        let shared = cache_indexed_file(
                            shared,
                            &mut text_file,
                            provider,
                            syntax_limits,
                            content_cache,
                            limits.files_max(),
                        );
                        Ok(IndexRead::Included(shared))
                    }
                    None => {
                        if let Some(syntax) = cached_syntax {
                            let file = Arc::new(IndexedFile::new_with_shared_syntax(
                                text_file.path().clone(),
                                Arc::clone(&text_file.content),
                                digest,
                                text_file.executable(),
                                syntax,
                            ));
                            Ok(IndexRead::Included(cache_indexed_file(
                                file,
                                &mut text_file,
                                provider,
                                syntax_limits,
                                content_cache,
                                limits.files_max(),
                            )))
                        } else {
                            match syntax_read(&text_file, path, provider, syntax_limits)
                                .map(shared_read)?
                            {
                                IndexRead::Included(file) => {
                                    Ok(IndexRead::Included(cache_indexed_file(
                                        file,
                                        &mut text_file,
                                        provider,
                                        syntax_limits,
                                        content_cache,
                                        limits.files_max(),
                                    )))
                                }
                                IndexRead::Skipped(warning) => Ok(IndexRead::Skipped(warning)),
                            }
                        }
                    }
                };
                IndexRead::Included(Self { text_file, parsed })
            }
            IndexRead::Skipped(warning) => IndexRead::Skipped(warning),
        })
    }
}

/// One syntax outcome, holding an included file behind the `Arc` a publication shares it
/// through.
fn shared_read(read: IndexRead<IndexedFile>) -> IndexRead<Arc<IndexedFile>> {
    match read {
        IndexRead::Included(file) => IndexRead::Included(Arc::new(file)),
        IndexRead::Skipped(warning) => IndexRead::Skipped(warning),
    }
}

fn cache_indexed_file(
    file: Arc<IndexedFile>,
    text_file: &mut TextSourceFile,
    provider: &dyn SyntaxProvider,
    syntax_limits: SyntaxLimits,
    cache: &WorkspaceContentCache,
    entries_max: usize,
) -> Arc<IndexedFile> {
    let (source, syntax) = cache.insert(
        file.digest(),
        syntax_limits,
        provider,
        file.source_content(),
        file.syntax_facts(),
        entries_max,
    );
    text_file.content = Arc::clone(&source);
    if Arc::ptr_eq(file.source_content(), &source) && Arc::ptr_eq(file.syntax_facts(), &syntax) {
        file
    } else {
        Arc::new(IndexedFile::new_with_shared_syntax(
            text_file.path().clone(),
            source,
            file.digest(),
            text_file.executable(),
            syntax,
        ))
    }
}

/// Reads one baseline catalog candidate with bounded binary detection, counting it
/// against `workspace_bytes_max`.
fn read_catalog_file(
    root: &Path,
    path: &Path,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<IndexRead<TextSourceFile>, RiftError> {
    let read = catalog_file(root, path, limits)?;
    if let IndexRead::Included(file) = &read {
        count_workspace_bytes(workspace_bytes, file.content().len(), path, limits)?;
    }
    Ok(read)
}

/// Reads one baseline catalog candidate with bounded binary detection, counting no bytes.
fn catalog_file(
    root: &Path,
    path: &Path,
    limits: WorkspaceIndexLimits,
) -> Result<IndexRead<TextSourceFile>, RiftError> {
    let handle = fs::File::open(path).map_err(|error| catalog_io_error(path, error))?;
    let metadata = handle
        .metadata()
        .map_err(|error| catalog_io_error(path, error))?;
    let bytes = read_file_bytes(handle, path, limits)?;
    let project_path = project_path_below(root, path)?;
    if bytes.len() > limits.file_bytes_max() {
        let error = errors::index::workspace_file_too_large()
            .path(&project_path)
            .field("source.file_bytes")
            .observed(bytes.len())
            .maximum(limits.file_bytes_max())
            .error();
        return Ok(IndexRead::left_out(WorkspaceIndexWarning::FileTooLarge {
            path: project_path,
            error: Arc::new(error),
        }));
    }
    if bytes.contains(&0) {
        return Ok(IndexRead::left_out(WorkspaceIndexWarning::BinarySource(
            project_path,
        )));
    }
    if std::str::from_utf8(&bytes).is_err() {
        let source = std::str::from_utf8(&bytes).expect_err("invalid UTF-8 was detected");
        let error = errors::index::workspace_invalid_source()
            .path(&project_path)
            .source(source)
            .error();
        return Ok(IndexRead::left_out(
            WorkspaceIndexWarning::InvalidUtf8Source {
                path: project_path,
                error: Arc::new(error),
            },
        ));
    }
    let content = source_utf8(bytes, path)?;
    let mut file = TextSourceFile::from_content(project_path, content);
    file.executable = metadata_is_executable(&metadata);
    Ok(IndexRead::Included(file))
}

fn catalog_io_error(path: &Path, source: std::io::Error) -> RiftError {
    if source.kind() == std::io::ErrorKind::NotFound {
        errors::index::workspace_changed_during_capture()
            .path(path)
            .source(source)
            .error()
    } else {
        errors::index::workspace_filesystem()
            .path(path)
            .source(source)
            .error()
    }
}

#[cfg(unix)]
pub(crate) fn metadata_is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(crate) fn metadata_is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

/// Reads at most one byte beyond the file bound, so callers can classify an oversized
/// file without reading its remaining bytes. Capture, catalog, and syntax reads share it.
pub(crate) fn read_file_bytes(
    reader: impl std::io::Read,
    path: &Path,
    limits: WorkspaceIndexLimits,
) -> Result<Vec<u8>, RiftError> {
    let mut bytes = Vec::new();
    reader
        .take(limits.file_bytes_max().saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            errors::index::workspace_filesystem()
                .path(path)
                .source(error)
                .error()
        })?;
    Ok(bytes)
}

/// Includes one UTF-8 file in the baseline content catalog.
pub(crate) fn included_text_file(
    project_path: ProjectPath,
    bytes: Vec<u8>,
    context_path: &Path,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<TextSourceFile, RiftError> {
    count_workspace_bytes(workspace_bytes, bytes.len(), context_path, limits)?;
    let content = source_utf8(bytes, context_path)?;
    Ok(TextSourceFile::from_content(project_path, content))
}

/// Decides whether bytes read for one claimed file are valid UTF-8 source: the single
/// classification source discovery's request-time capture, index construction, and every
/// direct file read share. Invalid bytes refuse; the caller decides whether that refusal
/// fails its own operation outright (a single-file read) or is instead treated as an
/// omission (a whole-workspace build or capture).
fn source_utf8(bytes: Vec<u8>, context_path: &Path) -> Result<String, RiftError> {
    String::from_utf8(bytes).map_err(|error| {
        errors::index::workspace_invalid_source()
            .path(context_path)
            .source(error)
            .error()
    })
}

/// The project-relative address of `absolute`, which the caller has already proven lies
/// below `root`. Every direct filesystem read into the index shares this conversion, so a
/// discovered path and a path recovered after a skipped read resolve to the same
/// [`ProjectPath`].
fn project_path_below(root: &Path, absolute: &Path) -> Result<ProjectPath, RiftError> {
    let relative = absolute.strip_prefix(root).map_err(|error| {
        errors::index::workspace_invalid_path()
            .path(absolute)
            .source(error)
            .error()
    })?;
    relative_path(relative)
}

/// The [`ProjectPath`] one path relative to the workspace root spells, its components joined
/// with `/` on every platform.
///
/// # Errors
///
/// Returns [`RiftError`] when a component is not UTF-8 or the joined spelling is not
/// a valid project path.
pub fn relative_path(path: &Path) -> Result<ProjectPath, RiftError> {
    let value = path
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| errors::index::workspace_invalid_path().path(path).error())?
        .join("/");
    ProjectPath::new(value).map_err(|error| {
        errors::index::workspace_invalid_path()
            .path(path)
            .cause(error)
            .error()
    })
}

/// The class one declaration's names reach against a lowercase query.
///
/// The classing itself lives in `rift-ranking`, so the global API client checks a
/// package hit's class under the same rule this index orders by.
fn symbol_rank(symbol: &SyntaxSymbol, query: &str) -> Option<IdentifierMatchClass> {
    match_class(
        query,
        &symbol.name.to_lowercase(),
        &symbol.qualified_name.to_lowercase(),
    )
}

/// One declaration's ranking identity: the same `SymbolId` a read answers with, so a
/// ranked identity and a read address are one value.
#[must_use]
pub fn declaration_identity(matched: SymbolMatch<'_>) -> DocumentIdentity {
    let identity = symbol_identity(
        &matched.file.syntax().language().identity_segment(),
        matched.file.path().as_str(),
        &matched.symbol.qualified_name,
    );
    DocumentIdentity::new(identity).unwrap_or_else(|error| {
        unreachable!("a declaration's rift identity must be non-empty: error={error}")
    })
}

/// One index document for a symbol declaration. `identity` is minted by the same
/// [`rift_core::symbol_identity`] the read service uses for that declaration's wire
/// `SymbolId`, so a lexical hit's identity equals the id `get_symbol` returns for it.
///
/// Every field a provider published lands in its own column. A provider that publishes
/// no signature leaves that field absent. The declaration's source stays out: it is a
/// range of its file's text, which the file's own document stores and indexes once, and
/// a query word only its body holds reaches it through that file row.
fn symbol_document(
    file: &IndexedFile,
    symbol: &SyntaxSymbol,
    left_out: &mut LeftOut,
) -> Option<IndexDocument> {
    let identity = rift_core::symbol_identity(
        &file.syntax().language().identity_segment(),
        file.path().as_str(),
        &symbol.qualified_name,
    );
    document(
        identity,
        file.path(),
        DocumentKind::Symbol,
        declaration_fields(symbol),
        left_out,
    )
}

fn symbol_document_key(file: &IndexedFile, limits: SyntaxLimits) -> SymbolDocumentKey {
    SymbolDocumentKey {
        digest: file.digest(),
        language: file.syntax().language().identity_segment(),
        source_bytes_max: limits.source_bytes_max(),
        syntax_nodes_max: limits.syntax_nodes_max(),
        syntax_depth_max: limits.syntax_depth_max(),
    }
}

fn carried_symbol_documents(
    previous: Option<&WorkspaceIndex>,
    files: &BTreeMap<ProjectPath, Arc<IndexedFile>>,
    limits: SyntaxLimits,
) -> RwLock<BTreeMap<ProjectPath, CachedSymbolDocuments>> {
    let mut carried = BTreeMap::new();
    if let Some(previous) = previous.filter(|previous| previous.limits.syntax() == limits) {
        let previous_cache = previous
            .symbol_documents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (path, file) in files {
            let shared_file = previous
                .files
                .get(path)
                .is_some_and(|previous_file| Arc::ptr_eq(previous_file, file));
            if shared_file && let Some(cached) = previous_cache.get(path) {
                carried.insert(path.clone(), cached.clone());
            }
        }
    }
    RwLock::new(carried)
}

/// The searchable fields one declaration fills: equal bytes produce equal fields and one
/// digest.
#[must_use]
pub(crate) fn declaration_fields(symbol: &SyntaxSymbol) -> DocumentFields {
    let containers = symbol.container.iter().map(String::as_str);
    let terms = identifier_terms(
        [symbol.name.as_str(), symbol.qualified_name.as_str()]
            .into_iter()
            .chain(containers),
        IDENTIFIER_TERMS_BYTES_MAX,
    );
    DocumentFields::empty()
        .with(SearchableField::Name, symbol.name.clone())
        .with(
            SearchableField::QualifiedName,
            symbol.qualified_name.clone(),
        )
        .with(SearchableField::IdentifierTerms, terms)
        .with(SearchableField::Signature, rendered_signatures(symbol))
        .with(
            SearchableField::Documentation,
            attached_documentation(symbol),
        )
}

/// The declaration's rendered signatures, one per line.
///
/// A declaration the grammar marks callable in several forms renders each of them, so a
/// caller searching for a parameter name reaches the form that declares it.
fn rendered_signatures(symbol: &SyntaxSymbol) -> String {
    bounded_field(
        &symbol
            .signatures
            .iter()
            .map(|signature| signature.display.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        SIGNATURE_BYTES_MAX,
    )
}

/// The doc comments the grammar attached, one per line, comment syntax already stripped.
fn attached_documentation(symbol: &SyntaxSymbol) -> String {
    bounded_field(
        &symbol
            .documentation
            .iter()
            .map(|documentation| documentation.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        DOCUMENTATION_BYTES_MAX,
    )
}

/// Cuts one short field to its byte bound at a character boundary, so a truncated field is
/// still text and never a broken code point.
///
/// Only the fields the document shape bounds go through this. A declaration's source and
/// a text file's content do not: the store carries the operator's own byte bound over
/// those, and a document past it is left out of the index and recorded rather than
/// silently shortened.
fn bounded_field(value: &str, bytes_max: usize) -> String {
    if value.len() <= bytes_max {
        return value.to_owned();
    }
    let mut end = bytes_max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Assembles one project document, or leaves it out when its address or one of its short
/// fields runs past what the document shape accepts.
///
/// Percent-encoding can widen a path or a qualified name past the wire's own address
/// ceiling, so this is reachable from a legal workspace rather than a programmer error.
/// The file keeps answering `get_symbol` and identifier search; only its place in the
/// searchable corpus is absent, and the log says which path lost it.
pub(crate) fn document(
    identity: String,
    path: &ProjectPath,
    kind: DocumentKind,
    fields: DocumentFields,
    left_out: &mut LeftOut,
) -> Option<IndexDocument> {
    let digest = fields.digest();
    let built = DocumentIdentity::new(identity).and_then(|identity| {
        IndexDocument::new(
            identity,
            DocumentLocation::Project(path.clone()),
            kind,
            digest,
            fields,
        )
    });
    match built {
        Ok(document) => Some(document),
        Err(error) => {
            left_out.record(path, &error);
            None
        }
    }
}

/// What one publication pass left out of the searchable corpus.
///
/// The pass counts rather than records each refusal: a workspace whose paths all
/// breach the ceiling would otherwise write one record per declaration and evict
/// everything else the pass recorded from the bounded log store.
#[derive(Debug, Default)]
pub(crate) struct LeftOut {
    count: usize,
    first: Option<(String, String)>,
}

impl LeftOut {
    /// Counts one refusal, keeping the first path and violation for the record.
    pub(crate) fn record(&mut self, path: &ProjectPath, error: &RiftError) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some((path.as_str().to_owned(), error.to_string()));
        }
    }

    /// Raises one record for the whole pass, or none when the pass left nothing
    /// out.
    fn report(&self) {
        let Some((path, error)) = self.first.as_ref() else {
            return;
        };
        rift_tracing::warn!(
            component = "index",
            operation = "index.build",
            left_out = self.count,
            path = path.as_str(),
            error = error.as_str(),
            "declarations left out of the search index: an address or a field runs past \
             the document shape; the count is this pass's total and the path is the first \
             of them, and every file still answers get_symbol and identifier search"
        );
    }
}

/// The final segment of `path`, including its extension, when it has one.
///
/// A file document's `name` is the file name a caller would type, extension and all, so
/// `vision.mdx` reaches its file. The host-absolute root never enters it: two clones of
/// one tree publish the same name.
pub(crate) fn file_name(path: &ProjectPath) -> Option<String> {
    Path::new(path.as_str())
        .file_name()
        .and_then(OsStr::to_str)
        .map(str::to_owned)
}

fn is_notebook_path(path: &ProjectPath) -> bool {
    Path::new(path.as_str()).extension().and_then(OsStr::to_str) == Some("ipynb")
}

/// Whether `content_bytes` bytes of text-file content exceed `chunk_bytes_max`, the bound
/// past which [`WorkspaceIndex::lexical_units`] chunks the file instead of publishing it
/// whole.
fn exceeds_chunk_bound(content_bytes: usize, chunk_bytes_max: u64) -> bool {
    u64::try_from(content_bytes).unwrap_or(u64::MAX) > chunk_bytes_max
}

/// Whether `content` holds a line, its ending included, longer than `chunk_bytes_max`: the
/// line the chunking kernel cuts at a character boundary rather than keeping whole. A
/// shorter text holds no such line, so only a text past the bound is read.
fn holds_line_past(content: &str, chunk_bytes_max: usize) -> bool {
    content.len() > chunk_bytes_max
        && rift_core::line::lines_inclusive(content).any(|line| line.len() > chunk_bytes_max)
}

/// Widens an already-accepted `[search.text].max_chunk` bound (1kb to 16mb) into the `usize`
/// domain the chunking kernel indexes with.
fn checked_chunk_bytes_max(chunk_bytes_max: u64) -> usize {
    usize::try_from(chunk_bytes_max).unwrap_or_else(|_| {
        unreachable!(
            "an accepted max_chunk bound must fit usize on supported platforms: \
             chunk_bytes_max={chunk_bytes_max}"
        )
    })
}

/// Appends one text file's documents to `documents`: one whole document within
/// `chunk_bytes_max`, one per chunk otherwise, every chunk sharing the file's real path
/// and its file name so a hit still maps back to the file it came from. Each document
/// records where its text starts in the file, so a position inside a chunk maps back to
/// the file without chunking it again.
fn push_text_documents(
    documents: &mut Vec<IndexDocument>,
    file: &TextSourceFile,
    chunk_bytes_max: u64,
    left_out: &mut LeftOut,
) {
    let name = file_name(file.path());
    if !exceeds_chunk_bound(file.content().len(), chunk_bytes_max) {
        documents.extend(
            text_document(
                file.path().as_str().to_owned(),
                file,
                name.as_deref(),
                file.content(),
                left_out,
            )
            .map(|document| document.at_byte_offset(0)),
        );
        return;
    }
    let chunks = text_chunks(file.content(), checked_chunk_bytes_max(chunk_bytes_max));
    let mut previous_offset: Option<u64> = None;
    for (index, chunk) in chunks.iter().enumerate() {
        if let Some(previous) = previous_offset {
            let current = chunk.byte_offset();
            let path = file.path().as_str();
            assert!(
                current > previous,
                "chunk offsets must increase: previous={previous}, current={current}, path={path}"
            );
        }
        previous_offset = Some(chunk.byte_offset());
        let identity = format!("{}#{index}", file.path().as_str());
        documents.extend(
            text_document(identity, file, name.as_deref(), chunk.content(), left_out)
                .map(|document| document.at_byte_offset(chunk.byte_offset())),
        );
    }
}

/// Constructs one text-file document: its file name in `name`, its derived terms beside
/// it, and its text in `file_content`. A text file declares nothing, so the declaration
/// fields stay absent.
fn text_document(
    identity: String,
    file: &TextSourceFile,
    name: Option<&str>,
    content: &str,
    left_out: &mut LeftOut,
) -> Option<IndexDocument> {
    let terms = name.map_or_else(String::new, |name| {
        identifier_terms([name], IDENTIFIER_TERMS_BYTES_MAX)
    });
    let fields = DocumentFields::empty()
        .with_optional(SearchableField::Name, name)
        .with(SearchableField::IdentifierTerms, terms)
        .with(SearchableField::FileContent, content);
    document(
        identity,
        file.path(),
        DocumentKind::TextFile,
        fields,
        left_out,
    )
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use rift_syntax::SyntaxDocument;
    use rift_syntax::{RustSyntaxProvider, SyntaxLimits};

    fn error_path(error: &RiftError) -> Option<PathBuf> {
        error
            .context()
            .find_map(|(key, value)| (key == "path").then(|| PathBuf::from(value)))
    }

    fn warning_matches(
        warning: &WorkspaceIndexWarning,
        path: &ProjectPath,
        slug: ErrorSlug,
    ) -> bool {
        let (warning_path, error) = match warning {
            WorkspaceIndexWarning::InvalidUtf8Source { path, error }
            | WorkspaceIndexWarning::FileTooLarge { path, error }
            | WorkspaceIndexWarning::SyntaxTooLarge { path, error }
            | WorkspaceIndexWarning::Contribution { path, error } => (path, error),
            WorkspaceIndexWarning::BinarySource(_)
            | WorkspaceIndexWarning::DeclarationsBeyondBound(_) => return false,
        };
        warning_path == path
            && (error.slug() == slug
                || source_rift_error(error).is_some_and(|source| source.slug() == slug))
    }

    fn limit_evidence(error: &RiftError) -> Option<(String, u64, u64)> {
        let mut field = None;
        let mut observed = None;
        let mut maximum = None;
        for (key, value) in error.context() {
            match key {
                "field" => field = Some(value),
                "observed" => observed = value.parse().ok(),
                "maximum" => maximum = value.parse().ok(),
                _ => {}
            }
        }
        Some((field?, observed?, maximum?))
    }

    #[test]
    fn shared_content_cache_reuses_facts_and_releases_them_after_workspace_drop() {
        let source = "pub fn beacon() {}\n";
        let first_root = tempfile::tempdir().expect("first workspace");
        let second_root = tempfile::tempdir().expect("second workspace");
        let changed_root = tempfile::tempdir().expect("changed workspace");
        let final_root = tempfile::tempdir().expect("final workspace");
        fs::create_dir_all(first_root.path().join("src")).expect("first source directory");
        fs::create_dir_all(second_root.path().join("lib")).expect("second source directory");
        fs::create_dir_all(changed_root.path().join("other")).expect("changed source directory");
        fs::create_dir_all(final_root.path().join("next")).expect("final source directory");
        fs::write(first_root.path().join("src/first.rs"), source).expect("first source");
        fs::write(second_root.path().join("lib/second.rs"), source).expect("same source");
        fs::write(changed_root.path().join("other/changed.rs"), source).expect("same bytes");
        fs::write(
            final_root.path().join("next/final.rs"),
            "pub fn lantern() {}\n",
        )
        .expect("new source");

        let cache = WorkspaceContentCache::default();
        let visibility = SourceVisibility::default();
        let text = TextFileInclusion::default();
        let languages = LanguageFileSelections::default();
        let limits = WorkspaceIndexLimits::default();
        let build = |root: &Path, limits, cache: &WorkspaceContentCache| {
            WorkspaceIndex::build_with_languages_cancellable_and_cache(
                root,
                limits,
                &visibility,
                &text,
                &languages,
                cache,
                &|| false,
            )
            .expect("workspace builds")
        };

        let first = build(first_root.path(), limits, &cache);
        let first_path = ProjectPath::new("src/first.rs").expect("first project path");
        let first_file = first.file(&first_path).expect("first indexed file");
        let source_weak = Arc::downgrade(first_file.source_content());
        let syntax_weak = Arc::downgrade(first_file.syntax_facts());

        let second = build(second_root.path(), limits, &cache);
        let second_path = ProjectPath::new("lib/second.rs").expect("second project path");
        let second_file = second.file(&second_path).expect("second indexed file");
        assert!(Arc::ptr_eq(
            first_file.source_content(),
            second_file.source_content()
        ));
        assert!(Arc::ptr_eq(
            first_file.syntax_facts(),
            second_file.syntax_facts()
        ));
        assert_eq!(first_file.path().as_str(), "src/first.rs");
        assert_eq!(second_file.path().as_str(), "lib/second.rs");
        assert_eq!(first_file.syntax().symbols()[0].name, "beacon");
        assert_eq!(first_file.source(), source);

        let syntax = limits.syntax();
        let changed_syntax = SyntaxLimits::new(
            syntax.source_bytes_max() - 1,
            syntax.syntax_nodes_max(),
            syntax.syntax_depth_max(),
        )
        .expect("changed syntax bound");
        let changed_limits = limits.with_syntax(changed_syntax);
        let changed = build(changed_root.path(), changed_limits, &cache);
        let changed_path = ProjectPath::new("other/changed.rs").expect("changed project path");
        let changed_file = changed.file(&changed_path).expect("changed indexed file");
        assert!(!Arc::ptr_eq(
            first_file.source_content(),
            changed_file.source_content()
        ));
        assert!(!Arc::ptr_eq(
            first_file.syntax_facts(),
            changed_file.syntax_facts()
        ));
        assert_eq!(first_file.syntax().symbols()[0].name, "beacon");

        drop(first);
        drop(second);
        drop(changed);
        assert!(source_weak.upgrade().is_none());
        assert!(syntax_weak.upgrade().is_none());
        assert_eq!(cache.entry_count(), 2);

        let final_limits = WorkspaceIndexLimits::new(
            1,
            limits.file_bytes_max(),
            limits.workspace_bytes_max(),
            limits.directory_depth_max(),
            limits.results_max(),
        )
        .expect("one-file bound");
        let final_index = build(final_root.path(), final_limits, &cache);
        assert_eq!(cache.entry_count(), 1);
        assert_eq!(
            final_index
                .files()
                .next()
                .expect("final indexed file")
                .syntax()
                .symbols()[0]
                .name,
            "lantern"
        );
    }

    fn assert_symbol_groups_match_index(index: &WorkspaceIndex, groups: &[Arc<[IndexDocument]>]) {
        assert_eq!(
            groups
                .iter()
                .flat_map(|group| group.iter().cloned())
                .collect::<Vec<_>>(),
            index
                .index_documents()
                .into_iter()
                .filter(|document| document.kind() == DocumentKind::Symbol)
                .collect::<Vec<_>>(),
            "grouped cache preserves symbol unit fields and order"
        );
    }

    fn assert_symbol_documents_do_not_carry_for_path_or_language(
        index: &WorkspaceIndex,
        path: &ProjectPath,
        file: &IndexedFile,
        limits: SyntaxLimits,
    ) {
        let moved_path = ProjectPath::new("moved.rs").expect("moved path");
        let moved_file = Arc::new(IndexedFile::new_with_shared_syntax(
            moved_path.clone(),
            Arc::clone(file.source_content()),
            file.digest(),
            file.executable(),
            Arc::clone(file.syntax_facts()),
        ));
        let moved = BTreeMap::from([(moved_path, moved_file)]);
        assert!(
            carried_symbol_documents(Some(index), &moved, limits)
                .read()
                .expect("document cache lock")
                .is_empty()
        );

        let python = registry::provider_for_extension("py").expect("Python provider");
        let python_facts = python
            .analyze(
                SyntaxSource {
                    path,
                    text: file.source(),
                },
                limits,
            )
            .expect("Python provider accepts source")
            .into_facts();
        let different_language = Arc::new(IndexedFile::new_with_shared_syntax(
            path.clone(),
            Arc::clone(file.source_content()),
            file.digest(),
            file.executable(),
            python_facts,
        ));
        let other_language = BTreeMap::from([(path.clone(), different_language)]);
        assert!(
            carried_symbol_documents(Some(index), &other_language, limits)
                .read()
                .expect("document cache lock")
                .is_empty()
        );
    }

    #[test]
    fn vector_symbol_documents_share_unchanged_files_only() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        fs::write(root.join("a.rs"), "pub fn alpha() {}\n").expect("first source");
        fs::write(root.join("b.rs"), "pub fn bravo() {}\n").expect("second source");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = TextFileInclusion::default();
        let first =
            WorkspaceIndex::build(root, limits, &visibility, &inclusion).expect("workspace builds");

        assert!(
            first
                .symbol_documents
                .read()
                .expect("document cache lock")
                .is_empty()
        );
        let lexical = first.index_documents();
        assert_eq!(
            lexical
                .iter()
                .filter(|document| document.kind() == DocumentKind::Symbol)
                .count(),
            2
        );
        assert!(
            first
                .symbol_documents
                .read()
                .expect("document cache lock")
                .is_empty()
        );

        let first_groups = first.symbol_index_documents_by_file();
        assert_eq!(first_groups.len(), 2);
        let original_path = ProjectPath::new("a.rs").expect("original path");
        let original_file = first.file(&original_path).expect("indexed file");
        assert_symbol_documents_do_not_carry_for_path_or_language(
            &first,
            &original_path,
            original_file,
            limits.syntax(),
        );

        let changed_path = ProjectPath::new("b.rs").expect("changed path");
        let changed_content = "pub fn charlie() {}\n";
        fs::write(root.join("b.rs"), changed_content).expect("changed source");
        let changes = resolved(&first, root, &["b.rs"]);
        let rebuilt = first.rebuilt(&changes).expect("incremental rebuild");
        let rebuilt_groups = rebuilt.symbol_index_documents_by_file();

        assert!(Arc::ptr_eq(&first_groups[0], &rebuilt_groups[0]));
        assert!(!Arc::ptr_eq(&first_groups[1], &rebuilt_groups[1]));
        assert_symbol_groups_match_index(&rebuilt, &rebuilt_groups);
        assert_eq!(
            rebuilt
                .file(&changed_path)
                .expect("changed indexed file")
                .syntax()
                .symbols()[0]
                .name,
            "charlie"
        );

        let limits_changed = SyntaxLimits::new(
            limits.syntax().source_bytes_max(),
            limits.syntax().syntax_nodes_max() - 1,
            limits.syntax().syntax_depth_max(),
        )
        .expect("changed syntax bounds");
        let invalidated = carried_symbol_documents(Some(&rebuilt), &rebuilt.files, limits_changed);
        assert!(invalidated.read().expect("document cache lock").is_empty());
    }

    #[test]
    fn test_a_pass_counts_what_it_left_out_and_keeps_the_first_of_them() {
        // The pass raises one record however many declarations it refused. One
        // per declaration would fill the bounded log store from a workspace
        // whose declarations all breach the shape, and evict everything else
        // the pass recorded.
        let overlong = "n".repeat(rift_ranking::NAME_BYTES_MAX + 1);
        let mut left_out = LeftOut::default();
        for name in ["first", "second", "third"] {
            let path = ProjectPath::new(format!("src/{name}.rs")).expect("a legal path");
            let fields = DocumentFields::empty().with(SearchableField::QualifiedName, &overlong);
            assert!(
                document(
                    format!("rift://symbol/rust/src/{name}.rs#{overlong}"),
                    &path,
                    DocumentKind::Symbol,
                    fields,
                    &mut left_out,
                )
                .is_none(),
                "a field past the shape leaves the declaration out"
            );
        }
        assert_eq!(left_out.count, 3, "the pass counts every refusal");
        let (path, error) = left_out.first.as_ref().expect("the first refusal is kept");
        assert_eq!(path, "src/first.rs", "the record names the first of them");
        assert!(
            !error.contains(&overlong),
            "the refusal names the column and the counts, never the value: {error}"
        );
        left_out.report();
    }

    /// The project path one derived document is addressed by. Every document this
    /// index derives is a project document, so the package arm is unreachable here.
    fn document_path(document: &IndexDocument) -> &ProjectPath {
        match document.location() {
            DocumentLocation::Project(path) => path,
            DocumentLocation::Unit(unit) => {
                unreachable!("this index derives project documents alone: unit={unit}")
            }
        }
    }
    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("src")).expect("fixture directory");
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub struct Rift;\nimpl Rift { pub fn update() {} }\n",
        )
        .expect("fixture source");
        fs::write(directory.path().join("README.txt"), "ignored").expect("fixture prose");
        directory
    }

    fn prepared_fixture() -> (tempfile::TempDir, WorkspaceIndexPreparation, usize) {
        let directory = tempfile::tempdir().expect("temporary workspace");
        for file in 0..SOURCE_BATCH_FILES {
            fs::write(
                directory.path().join(format!("file-{file:04}.txt")),
                format!("file {file}\n"),
            )
            .expect("fixture file");
        }
        fs::create_dir_all(directory.path().join("src")).expect("source directory");
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn prepared_source() {}\n",
        )
        .expect("source fixture");
        fs::write(directory.path().join("Cargo.lock"), "version = 4\n").expect("lockfile fixture");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text_inclusion = TextFileInclusion::default();
        let languages = LanguageFileSelections::default();
        let mut preparation =
            WorkspaceIndexPreparation::new(directory.path(), limits, &text_inclusion, &languages)
                .expect("preparation configuration");
        let total = preparation
            .discover(&visibility, &|| false)
            .expect("selected paths");
        (directory, preparation, total)
    }

    fn assert_empty_and_selected(preparation: &WorkspaceIndexPreparation, total: usize) {
        let empty = preparation.empty_snapshot().expect("empty index");
        assert_eq!(empty.text_file_count(), 0);
        assert_eq!(preparation.prepared(), 0);
        assert_eq!(preparation.total(), Some(total));
        assert_eq!(total, SOURCE_BATCH_FILES + 2);
        let selected = preparation
            .selected_paths()
            .expect("selected path identities")
            .expect("discovery completed");
        assert_eq!(selected.len(), total);
        assert_eq!(selected.iter().collect::<BTreeSet<_>>().len(), total);
        assert_eq!(preparation.next_checkpoint(), Some(SOURCE_BATCH_FILES));
    }

    fn assert_partial_preparation(
        preparation: &mut WorkspaceIndexPreparation,
        total: usize,
    ) -> WorkspaceIndex {
        let partial = preparation
            .advance_to(SOURCE_BATCH_FILES, &|| false)
            .expect("first publication");
        assert_eq!(preparation.prepared(), SOURCE_BATCH_FILES);
        assert_eq!(partial.text_file_count(), SOURCE_BATCH_FILES);
        assert_eq!(partial.file_count(), 1);
        assert_eq!(
            preparation.prepared_paths().expect("prepared paths").len(),
            SOURCE_BATCH_FILES
        );
        assert_eq!(preparation.next_checkpoint(), Some(total));
        partial
    }

    fn assert_complete_preparation(
        directory: &Path,
        preparation: &mut WorkspaceIndexPreparation,
        total: usize,
        partial: &WorkspaceIndex,
    ) {
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text_inclusion = TextFileInclusion::default();
        let languages = LanguageFileSelections::default();
        let complete = preparation
            .advance_to(total, &|| false)
            .expect("complete publication");
        let fresh = WorkspaceIndex::build_with_languages(
            directory,
            limits,
            &visibility,
            &text_inclusion,
            &languages,
        )
        .expect("fresh index");
        assert_eq!(complete.text_file_count(), SOURCE_BATCH_FILES + 1);
        assert_eq!(preparation.prepared(), total);
        assert_eq!(preparation.next_checkpoint(), None);
        assert_eq!(complete.digests(), fresh.digests());
        assert_eq!(complete.index_documents(), fresh.index_documents());
        let source = ProjectPath::new("src/lib.rs").expect("source path");
        assert!(Arc::ptr_eq(
            partial.files.get(&source).expect("partial source"),
            complete.files.get(&source).expect("complete source"),
        ));
    }

    fn two_text_preparation() -> (tempfile::TempDir, WorkspaceIndexPreparation) {
        let directory = tempfile::tempdir().expect("temporary workspace");
        for name in ["first.txt", "second.txt"] {
            fs::write(directory.path().join(name), format!("{name}\n")).expect("fixture text");
        }
        let mut preparation = WorkspaceIndexPreparation::new(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        )
        .expect("preparation configuration");
        assert_eq!(
            preparation
                .discover(&SourceVisibility::default(), &|| false)
                .expect("selected paths"),
            2
        );
        (directory, preparation)
    }

    #[test]
    fn preparation_reuses_previous_documentation_when_target_arrives() {
        use rift_protocol::documentation::DocumentationLinkResolution;

        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::create_dir(root.join("docs")).expect("documentation directory");
        fs::write(
            root.join("docs/README.md"),
            "# Guide\n\n[Notes](notes.md)\n",
        )
        .expect("guide");
        fs::write(root.join("docs/notes.md"), "# Notes\n\nText.\n").expect("notes");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text_inclusion = TextFileInclusion::new(vec!["docs/*.md".to_owned()], 1_024);
        let languages = LanguageFileSelections::default();
        let mut preparation =
            WorkspaceIndexPreparation::new(root, limits, &text_inclusion, &languages)
                .expect("preparation configuration");
        assert_eq!(
            preparation
                .discover(&visibility, &|| false)
                .expect("selected paths"),
            2
        );

        let first = preparation
            .advance_to(1, &|| false)
            .expect("first publication");
        assert!(
            preparation.accepts_previous(&first),
            "same accepted inputs permit reuse"
        );
        let unresolved = |index: &WorkspaceIndex| {
            index.documentation().index().links.iter().any(|link| {
                link.authored == "notes.md"
                    && matches!(
                        link.resolution,
                        DocumentationLinkResolution::Unresolved { .. }
                    )
            })
        };
        assert!(
            unresolved(&first),
            "target has not arrived yet; prepared={:?}; sources={:?}; links={:?}",
            preparation.prepared_paths().expect("prepared paths"),
            first.documentation().index().sources,
            first.documentation().index().links
        );

        let complete = preparation
            .advance_to_with_previous(2, Some(&first), &|| false)
            .expect("complete publication");
        let cold = WorkspaceIndex::build_with_languages(
            root,
            limits,
            &visibility,
            &text_inclusion,
            &languages,
        )
        .expect("cold index");
        assert_eq!(
            complete.documentation().index(),
            cold.documentation().index()
        );
        assert_eq!(complete.index_documents(), cold.index_documents());
        assert!(complete.documentation().index().links.iter().any(|link| {
            link.authored == "notes.md"
                && matches!(
                    link.resolution,
                    DocumentationLinkResolution::Resolved { .. }
                )
        }));
        assert!(unresolved(&first), "earlier publication stays unchanged");
    }

    #[test]
    fn preparation_reuses_previous_semantics_when_source_arrives() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::create_dir(root.join("src")).expect("source directory");
        fs::write(root.join("src/a.rs"), "pub fn first() {}\n").expect("first source");
        fs::write(root.join("src/b.rs"), "pub fn second() {}\n").expect("second source");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text_inclusion = TextFileInclusion::default();
        let languages = LanguageFileSelections::default();
        let mut preparation =
            WorkspaceIndexPreparation::new(root, limits, &text_inclusion, &languages)
                .expect("preparation configuration");
        assert_eq!(
            preparation
                .discover(&visibility, &|| false)
                .expect("selected paths"),
            2
        );
        let first = preparation
            .advance_to(1, &|| false)
            .expect("first source publication");
        assert_eq!(first.symbols("first", 10).expect("first symbol").len(), 1);
        let complete = preparation
            .advance_to_with_previous(2, Some(&first), &|| false)
            .expect("complete source publication");
        let cold = WorkspaceIndex::build_with_languages(
            root,
            limits,
            &visibility,
            &text_inclusion,
            &languages,
        )
        .expect("cold index");
        for query in ["first", "second"] {
            let symbol_facts = |index: &WorkspaceIndex| {
                index
                    .symbols(query, 10)
                    .expect("symbol query")
                    .into_iter()
                    .map(|matched| {
                        (
                            matched.file.path().as_str().to_owned(),
                            matched.symbol.qualified_name.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(symbol_facts(&complete), symbol_facts(&cold));
        }
        assert_eq!(complete.index_documents(), cold.index_documents());
    }

    #[test]
    fn preparation_skips_reuse_when_text_selection_changes() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::write(root.join("README.md"), "# Guide\n").expect("guide");
        fs::write(root.join("notes.txt"), "Notes\n").expect("notes");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let languages = LanguageFileSelections::default();
        let prior_inclusion = TextFileInclusion::new(Vec::new(), 1_024);
        let prior = WorkspaceIndex::build_with_languages(
            root,
            limits,
            &visibility,
            &prior_inclusion,
            &languages,
        )
        .expect("prior index");
        let changed_inclusion = TextFileInclusion::new(vec!["**/*.txt".to_owned()], 1_024);
        let mut preparation =
            WorkspaceIndexPreparation::new(root, limits, &changed_inclusion, &languages)
                .expect("changed preparation configuration");
        let total = preparation
            .discover(&visibility, &|| false)
            .expect("selected paths");
        assert!(
            !preparation.accepts_previous(&prior),
            "changed text selection cannot reuse prior derived data"
        );
        let actual = preparation
            .advance_to_with_previous(total, Some(&prior), &|| false)
            .expect("changed-policy index");
        let cold = WorkspaceIndex::build_with_languages(
            root,
            limits,
            &visibility,
            &changed_inclusion,
            &languages,
        )
        .expect("cold changed-policy index");
        assert_eq!(actual.documentation().index(), cold.documentation().index());
        assert_eq!(actual.index_documents(), cold.index_documents());
    }

    #[test]
    fn preparation_rejects_previous_with_changed_root_limits_text_or_language() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        let limits = WorkspaceIndexLimits::default();
        let text_inclusion = TextFileInclusion::default();
        let languages = LanguageFileSelections::default();
        let mut preparation =
            WorkspaceIndexPreparation::new(root, limits, &text_inclusion, &languages)
                .expect("preparation configuration");
        let previous = preparation.empty_snapshot().expect("empty index");
        assert!(preparation.accepts_previous(&previous));

        preparation.root.push("other");
        assert!(!preparation.accepts_previous(&previous), "root must match");
        preparation.root = previous.root.clone();

        preparation.limits = limits
            .with_workspace_bounds(
                limits.files_max() - 1,
                limits.workspace_bytes_max(),
                limits.declarations_max(),
            )
            .expect("changed bounds");
        assert!(
            !preparation.accepts_previous(&previous),
            "bounds must match"
        );
        preparation.limits = limits;

        preparation.text_inclusion = TextFileInclusion::new(vec!["**/*.txt".to_owned()], 1_024);
        assert!(
            !preparation.accepts_previous(&previous),
            "text selection must match"
        );
        preparation.text_inclusion = text_inclusion;
        preparation.language = Arc::new(
            WorkspaceLanguagePolicy::build(
                &preparation.root,
                &languages,
                &preparation.text_inclusion,
            )
            .expect("compiled language policy"),
        );
        assert!(
            !preparation.accepts_previous(&previous),
            "compiled language policy must match"
        );
    }

    #[test]
    fn preparation_resumes_after_cancellation_with_completed_prefix() {
        let (directory, mut preparation) = two_text_preparation();
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let cancelled = || checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 4;
        let error = preparation
            .advance_to(2, &cancelled)
            .expect_err("cancellation stops before next text file");
        assert_eq!(error.slug(), errors::index::workspace_cancelled::SLUG);
        assert_eq!(preparation.prepared(), 1);
        let partial = preparation
            .advance_to(1, &|| false)
            .expect("completed prefix remains publishable");
        assert_eq!(partial.text_file_count(), 1);
        let complete = preparation
            .advance_to(2, &|| false)
            .expect("retry resumes at next file");
        let fresh = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("fresh index");
        assert_eq!(complete.digests(), fresh.digests());
    }

    #[test]
    fn preparation_resumes_after_a_discovered_file_returns() {
        let (directory, mut preparation) = two_text_preparation();
        let removed = directory.path().join("second.txt");
        fs::remove_file(&removed).expect("remove selected path");
        let error = preparation
            .advance_to(2, &|| false)
            .expect_err("a path removed after discovery names capture movement");
        assert_eq!(
            error.slug(),
            errors::index::workspace_changed_during_capture::SLUG
        );
        assert_eq!(preparation.prepared(), 1);
        let source = std::error::Error::source(&error).expect("original read cause");
        assert_eq!(
            source
                .downcast_ref::<std::io::Error>()
                .expect("filesystem cause")
                .kind(),
            std::io::ErrorKind::NotFound,
            "the returned capture movement retains its NotFound cause"
        );
        fs::write(&removed, "second.txt\n").expect("restore selected path");
        let complete = preparation
            .advance_to(2, &|| false)
            .expect("retry resumes at the changed path");
        let fresh = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("fresh index");
        assert_eq!(complete.digests(), fresh.digests());
    }

    #[test]
    fn workspace_byte_bound_refusal_keeps_previous_count() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let limits = WorkspaceIndexLimits::default()
            .with_workspace_bounds(
                WorkspaceIndexLimits::default().files_max(),
                4,
                WorkspaceIndexLimits::default().declarations_max(),
            )
            .expect("positive bounds");
        let path = directory.path().join("file.txt");
        let mut workspace_bytes = 3;
        let error = count_workspace_bytes(&mut workspace_bytes, 2, &path, limits)
            .expect_err("aggregate limit crossed");
        assert_eq!(
            error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
        assert_eq!(workspace_bytes, 3, "failed file does not advance count");
    }

    #[test]
    fn preparation_publishes_empty_partial_and_complete_indexes() {
        let (directory, mut preparation, total) = prepared_fixture();
        assert_empty_and_selected(&preparation, total);
        let partial = assert_partial_preparation(&mut preparation, total);
        let (partial_capture, last) = preparation
            .capture_prepared_paths(&LastCapture::default(), &|| false)
            .expect("capture prepared paths");
        assert_eq!(partial_capture, partial.digests());
        assert_complete_preparation(directory.path(), &mut preparation, total, &partial);
        let complete = preparation
            .advance_to(total, &|| false)
            .expect("complete index");
        let (complete_capture, _) = preparation
            .capture_prepared_paths(&last, &|| false)
            .expect("capture complete paths");
        assert_eq!(complete_capture, complete.digests());
    }

    #[test]
    fn preparation_retains_incremental_rebuild_of_a_changed_prepared_file() {
        let (directory, mut preparation) = two_text_preparation();
        let partial = preparation
            .advance_to(1, &|| false)
            .expect("first selected file");
        let (_, last) = preparation
            .capture_prepared_paths(&LastCapture::default(), &|| false)
            .expect("first selected capture");
        fs::write(directory.path().join("first.txt"), "updated text\n")
            .expect("rewrite prepared file");
        let (captured, _) = preparation
            .capture_prepared_paths(&last, &|| false)
            .expect("capture changed prepared file");
        let changes = PathChanges::between(&partial.digests(), &captured);
        assert_eq!(changes.len(), 1, "only rewritten file changed");
        let repaired = partial
            .rebuilt_cancellable(&changes, &|| false)
            .expect("incremental repair");
        preparation.retain_rebuilt_snapshot(&repaired);
        let complete = preparation
            .advance_to(2, &|| false)
            .expect("continue after repair");
        let fresh = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("fresh index");
        assert_eq!(complete.digests(), fresh.digests());
        assert_eq!(complete.text_file_count(), 2);
    }

    #[test]
    fn preparation_rebuild_retains_bytes_for_declaration_bound_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let first = b"pub fn alpha() {}\npub fn bravo() {}\n";
        let later = b"version = 3\n";
        let replacement = b"pub fn delta() {}\npub fn gamma() {}\npub fn epsilon() {}\n";
        fs::write(root.join("a.rs"), first)?;
        fs::write(root.join("Cargo.lock"), later)?;
        let limits = WorkspaceIndexLimits::default().with_workspace_bounds(
            WorkspaceIndexLimits::default().files_max(),
            replacement.len() + later.len() - 1,
            1,
        )?;
        let mut preparation = WorkspaceIndexPreparation::new(
            root,
            limits,
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        )?;
        assert_eq!(
            preparation.discover(&SourceVisibility::default(), &|| false)?,
            2
        );

        let partial = preparation.advance_to(1, &|| false)?;
        let first_path = ProjectPath::new("a.rs")?;
        assert!(partial.file(&first_path).is_none());
        assert!(matches!(
            partial
                .warnings()
                .iter()
                .find(|warning| warning.path() == &first_path),
            Some(WorkspaceIndexWarning::DeclarationsBeyondBound(_))
        ));
        let (before, last) =
            preparation.capture_prepared_paths(&LastCapture::default(), &|| false)?;
        assert!(PathChanges::between(&partial.digests(), &before).is_empty());

        fs::write(root.join("a.rs"), replacement)?;
        let cold_error = WorkspaceIndex::build(
            root,
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect_err("cold indexing must count declaration-refused source bytes");
        assert_eq!(
            cold_error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
        let (captured, _) = preparation.capture_prepared_paths(&last, &|| false)?;
        let changes = PathChanges::between(&partial.digests(), &captured);
        assert_eq!(changes.len(), 1);
        let rebuilt = partial.rebuilt_cancellable(&changes, &|| false)?;
        preparation.retain_rebuilt_snapshot(&rebuilt);

        let error = preparation
            .advance_to(2, &|| false)
            .expect_err("later lockfile must exceed bound after counted earlier source");
        assert_eq!(
            error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
        Ok(())
    }

    #[test]
    fn preparation_rebuild_releases_bytes_for_a_deleted_declaration_bound_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let first = b"pub fn alpha() {}\npub fn bravo() {}\n";
        let later = b"version = 3\n";
        fs::write(root.join("a.rs"), first)?;
        fs::write(root.join("Cargo.lock"), later)?;
        let limits = WorkspaceIndexLimits::default().with_workspace_bounds(
            WorkspaceIndexLimits::default().files_max(),
            first.len() + later.len() - 1,
            1,
        )?;
        let mut preparation = WorkspaceIndexPreparation::new(
            root,
            limits,
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        )?;
        assert_eq!(
            preparation.discover(&SourceVisibility::default(), &|| false)?,
            2
        );
        let partial = preparation.advance_to(1, &|| false)?;
        let first_path = ProjectPath::new("a.rs")?;
        assert!(partial.file(&first_path).is_none());

        fs::remove_file(root.join("a.rs"))?;
        let changes =
            PathChanges::resolve([(first_path.clone(), None)], |path| partial.record(path));
        let rebuilt = partial.rebuilt_cancellable(&changes, &|| false)?;
        preparation.retain_rebuilt_snapshot(&rebuilt);
        let complete = preparation.advance_to(2, &|| false)?;
        let fresh = WorkspaceIndex::build(
            root,
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        assert_eq!(complete.digests(), fresh.digests());
        Ok(())
    }

    #[test]
    fn rebuilt_counts_a_retained_lockfile_with_a_growing_refused_source()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let first = b"pub fn alpha() {}\npub fn bravo() {}\n";
        let later = b"version = 3\n";
        let replacement = b"pub fn delta() {}\npub fn gamma() {}\n// grow\n";
        fs::write(root.join("a.rs"), first)?;
        fs::write(root.join("Cargo.lock"), later)?;
        let limits = WorkspaceIndexLimits::default().with_workspace_bounds(
            WorkspaceIndexLimits::default().files_max(),
            first.len() + later.len(),
            1,
        )?;
        let previous = WorkspaceIndex::build(
            root,
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        assert_eq!(counted_bytes(&previous), first.len() + later.len());

        let source_path = ProjectPath::new("a.rs")?;
        fs::write(root.join("a.rs"), replacement)?;
        let changes = PathChanges::resolve(
            [(
                source_path.clone(),
                Some(FileRecord::Digest(FileDigest::of(replacement))),
            )],
            |path| previous.record(path),
        );
        let cold_error = WorkspaceIndex::build(
            root,
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect_err("cold indexing counts retained lockfile bytes");
        let rebuilt_error = previous
            .rebuilt_cancellable(&changes, &|| false)
            .expect_err("incremental rebuild counts retained lockfile bytes");
        assert_eq!(
            cold_error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
        assert_eq!(
            rebuilt_error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
        Ok(())
    }

    #[test]
    fn rebuilt_recounts_a_shrinking_refused_source() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let first = b"pub fn alpha() {}\npub fn bravo() {}\n";
        let later = b"version = 3\n";
        let replacement = b"pub fn kept() {}\n";
        fs::write(root.join("a.rs"), first)?;
        fs::write(root.join("Cargo.lock"), later)?;
        let limits = WorkspaceIndexLimits::default().with_workspace_bounds(
            WorkspaceIndexLimits::default().files_max(),
            first.len() + later.len(),
            1,
        )?;
        let previous = WorkspaceIndex::build(
            root,
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        let source_path = ProjectPath::new("a.rs")?;
        fs::write(root.join("a.rs"), replacement)?;
        let changes = PathChanges::resolve(
            [(
                source_path,
                Some(FileRecord::Digest(FileDigest::of(replacement))),
            )],
            |path| previous.record(path),
        );
        let rebuilt = previous.rebuilt_cancellable(&changes, &|| false)?;
        let cold = WorkspaceIndex::build(
            root,
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        assert_eq!(rebuilt.digests(), cold.digests());
        assert_eq!(counted_bytes(&rebuilt), replacement.len() + later.len());
        Ok(())
    }

    /// Builds one index over `directory` under the default policies.
    fn indexed(directory: &Path, text_inclusion: &TextFileInclusion) -> WorkspaceIndex {
        WorkspaceIndex::build(
            directory,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            text_inclusion,
        )
        .expect("the fixture workspace must index")
    }

    /// Every byte baseline catalog counts against aggregate bound.
    fn counted_bytes(index: &WorkspaceIndex) -> usize {
        WorkspaceIndex::indexed_bytes(&index.files, &index.text_files, &index.left_out)
    }

    /// Resolves the paths named against `index`, reading each one's current bytes.
    fn resolved(index: &WorkspaceIndex, root: &Path, names: &[&str]) -> PathChanges {
        let observed = names.iter().map(|name| {
            let path = ProjectPath::new(*name).expect("fixture path must be valid");
            let digest = fs::read(root.join(name))
                .ok()
                .map(|bytes| FileRecord::Digest(FileDigest::of(&bytes)));
            (path, digest)
        });
        PathChanges::resolve(observed, |path| index.record(path))
    }

    #[test]
    fn test_policy_and_index_agree_on_a_tree_with_a_nested_ignore_file() {
        // `ruff` writes `.ruff_cache/.gitignore` holding `*` into any workspace it runs in.
        // Read against the workspace root that pattern excludes every file; read against
        // its own directory it excludes only the cache. The policy the watcher consults and
        // the walk the index runs have to reach the same verdict, or the watcher goes deaf
        // while the index stays full.
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("src")).expect("fixture directory");
        fs::create_dir_all(root.join(".ruff_cache")).expect("fixture directory");
        fs::write(root.join(".ruff_cache/.gitignore"), "*\n").expect("fixture ignore file");
        fs::write(root.join(".ruff_cache/cached.rs"), "pub fn cached() {}\n").expect("cache");
        fs::write(root.join("src/lib.rs"), "pub fn kept() {}\n").expect("fixture source");

        let index = indexed(root, &TextFileInclusion::default());
        let policy = WorkspaceSourcePolicy::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("the fixture policy must compile");

        let canonical = root.canonicalize().expect("canonical fixture root");
        assert!(
            index
                .file(&ProjectPath::new("src/lib.rs").expect("fixture path"))
                .is_some(),
            "the walk keeps a source file the nested ignore file does not name"
        );
        assert!(
            policy.visible(&canonical.join("src/lib.rs")),
            "the policy keeps the same file the walk kept"
        );
        assert!(
            index
                .file(&ProjectPath::new(".ruff_cache/cached.rs").expect("fixture path"))
                .is_none(),
            "the walk drops what the nested ignore file names"
        );
        assert!(
            !policy.visible(&canonical.join(".ruff_cache/cached.rs")),
            "the policy drops the same file the walk dropped"
        );
    }

    #[test]
    fn test_capture_and_index_fingerprint_one_tree_the_same_way() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("docs")).expect("fixture directory");
        fs::create_dir_all(root.join("docs-x")).expect("fixture directory");
        fs::write(root.join("docs/a.rs"), "pub fn a() {}\n").expect("fixture source");
        fs::write(root.join("docs-x/a.rs"), "pub fn b() {}\n").expect("fixture source");
        fs::write(root.join("docs/notes.txt"), "notes\n").expect("fixture prose");
        fs::write(root.join("zeta.rs"), "pub fn zeta() {}\n").expect("fixture source");
        let inclusion = TextFileInclusion::new(vec!["**".to_owned()], 1_024);

        let index = indexed(root, &inclusion);
        assert_eq!(
            index.text_file_count(),
            4,
            "catalog holds every visible file"
        );
        let captured = WorkspaceFingerprint::capture(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("fixture workspace must capture");
        assert_eq!(index.fingerprint(), &captured);
    }

    /// The documentation sources one index collected, by project path.
    fn documentation_paths(index: &WorkspaceIndex) -> Vec<String> {
        index
            .documentation()
            .index()
            .sources
            .iter()
            .filter_map(|source| match &source.identity.source {
                DocumentationSourceIdentity::Project { path } => Some(path.0.clone()),
                DocumentationSourceIdentity::Package { .. } => None,
            })
            .collect()
    }

    /// The index collects a workspace's documentation through the one documentation
    /// selection package analysis uses, and the `[documentation]` table the text
    /// inclusion carries overrides its defaults on every build path.
    #[test]
    fn test_documentation_follows_the_selection_the_text_inclusion_carries() {
        use rift_protocol::documentation::DocumentationConfiguration;
        use rift_protocol::read::PathPattern;

        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        fs::create_dir_all(root.join("docs/archive")).expect("fixture directory");
        fs::write(root.join("README.md"), "# Beacon\n").expect("fixture readme");
        fs::write(root.join("CHANGELOG.md"), "# Changes\n").expect("fixture change log");
        fs::write(root.join("docs/guide.md"), "# Guide\n").expect("fixture guide");
        fs::write(root.join("docs/archive/v1.md"), "# Version one\n").expect("fixture page");

        let index = indexed(root, &TextFileInclusion::default());
        assert_eq!(documentation_paths(&index), ["README.md", "docs/guide.md"]);

        let overridden =
            TextFileInclusion::default().with_documentation(DocumentationConfiguration {
                enabled: true,
                exclude: vec![PathPattern("README.md".to_owned())],
                force_include: vec![PathPattern("CHANGELOG.md".to_owned())],
            });
        let index = indexed(root, &overridden);
        assert_eq!(
            documentation_paths(&index),
            ["CHANGELOG.md", "docs/guide.md"]
        );

        fs::write(root.join("docs/guide.md"), "# Guide\n\nEdited.\n").expect("edited guide");
        let changes = resolved(&index, root, &["docs/guide.md"]);
        let next = index.rebuilt(&changes).expect("the rebuild must land");
        assert_eq!(
            documentation_paths(&next),
            ["CHANGELOG.md", "docs/guide.md"],
            "a rebuild keeps the table the index was built under"
        );
    }

    #[test]
    fn test_a_documentation_pattern_that_is_no_glob_refuses_the_build() {
        use rift_protocol::documentation::DocumentationConfiguration;
        use rift_protocol::read::PathPattern;

        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("README.md"), "# Beacon\n").expect("fixture readme");
        let inclusion =
            TextFileInclusion::default().with_documentation(DocumentationConfiguration {
                exclude: vec![PathPattern("docs/[guide".to_owned())],
                ..DocumentationConfiguration::default()
            });
        let error = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &inclusion,
        )
        .expect_err("an unclosed character class refuses the build");
        assert_eq!(error.slug(), errors::analysis::source_pattern_invalid::SLUG);
    }

    #[test]
    fn test_rebuilt_shares_every_file_the_change_set_does_not_name() {
        let directory = fixture();
        let root = directory.path();
        fs::write(root.join("src/other.rs"), "pub fn other() {}\n").expect("second source");
        let index = indexed(root, &TextFileInclusion::default());
        fs::write(root.join("src/lib.rs"), "pub struct Rift;\n").expect("edited source");

        let changes = resolved(&index, root, &["src/lib.rs", "src/other.rs"]);
        assert_eq!(changes.len(), 1, "only the edited file is named");
        let next = index.rebuilt(&changes).expect("the rebuild must land");

        let before = index
            .file(&ProjectPath::new("src/other.rs").expect("fixture path"))
            .expect("the untouched file is indexed");
        let after = next
            .file(&ProjectPath::new("src/other.rs").expect("fixture path"))
            .expect("the untouched file stays indexed");
        assert!(
            std::ptr::eq(before, after),
            "an untouched file is shared with the previous index rather than reparsed"
        );
        assert_eq!(
            next.file(&ProjectPath::new("src/lib.rs").expect("fixture path"))
                .expect("the edited file is indexed")
                .source(),
            "pub struct Rift;\n"
        );
        assert_ne!(
            index.fingerprint(),
            next.fingerprint(),
            "replacing one file's bytes changes workspace identity"
        );
    }

    #[test]
    fn test_rescanned_shares_every_file_whose_bytes_it_already_parsed() {
        let directory = fixture();
        let root = directory.path();
        fs::write(root.join("src/other.rs"), "pub fn other() {}\n").expect("second source");
        let index = indexed(root, &TextFileInclusion::default());
        fs::write(root.join("src/lib.rs"), "pub struct Rift;\n").expect("edited source");
        fs::create_dir_all(root.join("src/fresh")).expect("new directory");
        fs::write(root.join("src/fresh/mod.rs"), "pub fn fresh() {}\n").expect("new source");

        let next = index
            .rescanned(&SourceVisibility::default())
            .expect("the rescan must land");

        let path = |value: &str| ProjectPath::new(value).expect("fixture path");
        let before = index.file(&path("src/other.rs")).expect("indexed before");
        let after = next.file(&path("src/other.rs")).expect("indexed after");
        assert!(
            std::ptr::eq(before, after),
            "a file whose bytes did not move is shared rather than parsed again"
        );
        assert_eq!(
            next.file(&path("src/lib.rs"))
                .expect("edited file")
                .source(),
            "pub struct Rift;\n"
        );
        assert!(
            next.file(&path("src/fresh/mod.rs")).is_some(),
            "a new directory's file is read"
        );
        let fresh = indexed(root, &TextFileInclusion::default());
        assert_eq!(
            next.fingerprint(),
            fresh.fingerprint(),
            "a rescan and a full build of one tree agree"
        );
        assert_eq!(next.tree_revision(), fresh.tree_revision());
    }

    #[test]
    fn test_rescanned_reads_visibility_from_a_rewritten_ignore_file() {
        let directory = fixture();
        let root = directory.path();
        fs::write(root.join("src/other.rs"), "pub fn other() {}\n").expect("second source");
        let index = indexed(root, &TextFileInclusion::default());
        fs::write(root.join(".gitignore"), "src/other.rs\n").expect("ignore file");

        let next = index
            .rescanned(&SourceVisibility::default())
            .expect("the rescan must land");

        let path = |value: &str| ProjectPath::new(value).expect("fixture path");
        assert!(
            next.file(&path("src/other.rs")).is_none(),
            "the ignored file leaves"
        );
        assert!(std::ptr::eq(
            index.file(&path("src/lib.rs")).expect("indexed before"),
            next.file(&path("src/lib.rs")).expect("indexed after"),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_rescanned_parses_a_file_whose_mode_changed_again() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = fixture();
        let root = directory.path();
        let index = indexed(root, &TextFileInclusion::default());
        let lib = root.join("src/lib.rs");
        fs::set_permissions(&lib, fs::Permissions::from_mode(0o755)).expect("executable bit");

        let next = index
            .rescanned(&SourceVisibility::default())
            .expect("the rescan must land");

        let path = ProjectPath::new("src/lib.rs").expect("fixture path");
        let after = next.file(&path).expect("indexed after");
        assert!(!std::ptr::eq(
            index.file(&path).expect("indexed before"),
            after
        ));
        assert!(after.executable());
    }

    #[test]
    fn test_rebuilt_reindexes_an_edited_text_file() {
        let directory = fixture();
        let root = directory.path();
        let text_inclusion = TextFileInclusion::new(vec!["**".to_owned()], 1_024);
        let index = indexed(root, &text_inclusion);
        assert_eq!(index.text_file_count(), 2);
        fs::write(root.join("README.txt"), "edited prose").expect("edited text file");

        let changes = resolved(&index, root, &["README.txt"]);
        let next = index.rebuilt(&changes).expect("rebuild must land");

        let text_path = ProjectPath::new("README.txt").expect("fixture path");
        assert_eq!(
            next.text_file(&text_path)
                .expect("edited text file stays indexed")
                .content(),
            "edited prose"
        );
        assert_ne!(index.fingerprint(), next.fingerprint());
    }

    #[test]
    fn test_rebuilt_adds_removes_and_reclassifies_named_paths() {
        let directory = fixture();
        let root = directory.path();
        let text_inclusion = TextFileInclusion::new(vec!["**".to_owned()], 1_024);
        let index = indexed(root, &text_inclusion);
        assert_eq!(index.text_file_count(), 2);

        fs::write(root.join("src/added.rs"), "pub fn added() {}\n").expect("added source");
        fs::remove_file(root.join("README.txt")).expect("removed text file");
        let changes = resolved(&index, root, &["src/added.rs", "README.txt"]);
        let next = index.rebuilt(&changes).expect("rebuild must land");

        assert!(
            next.file(&ProjectPath::new("src/added.rs").expect("fixture path"))
                .is_some()
        );
        assert_eq!(next.text_file_count(), 2);
        assert_eq!(next.file_count(), index.file_count() + 1);
    }

    #[test]
    fn test_rebuilt_applied_twice_leaves_what_applying_it_once_left() {
        let directory = fixture();
        let root = directory.path();
        let index = indexed(root, &TextFileInclusion::default());
        fs::write(root.join("src/added.rs"), "pub fn added() {}\n").expect("added source");

        // Two rebuilds can be captured from one publication, so the second still calls the
        // path added after the first has written it. Both write what they read, and the
        // second replaces rather than adds to what the first left.
        let changes = resolved(&index, root, &["src/added.rs"]);
        let once = index
            .rebuilt(&changes)
            .expect("the first rebuild must land");
        let twice = once
            .rebuilt(&changes)
            .expect("the same change set must apply again");

        assert_eq!(once.file_count(), twice.file_count());
        assert_eq!(
            once.fingerprint(),
            twice.fingerprint(),
            "one change set applied twice leaves the tree it left once"
        );
    }

    #[test]
    fn test_rebuilt_counts_shared_files_against_the_aggregate_byte_bound() {
        let directory = fixture();
        let root = directory.path();
        let index = indexed(root, &TextFileInclusion::default());
        let indexed_bytes = counted_bytes(&index);
        let tight = WorkspaceIndexLimits::new(
            WORKSPACE_FILES_MAX_DEFAULT,
            1_048_576,
            indexed_bytes + 4,
            16,
            READ_RESULTS_MAX_DEFAULT,
        )
        .expect("a bound just above the indexed bytes is positive");
        let bounded = WorkspaceIndex::build(
            root,
            tight,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("the fixture fits the tight bound");

        fs::write(root.join("src/added.rs"), "pub fn added() {}\n").expect("added source");
        let changes = resolved(&bounded, root, &["src/added.rs"]);
        let error = bounded
            .rebuilt(&changes)
            .expect_err("the shared files already fill the aggregate bound");
        assert_eq!(
            error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
    }

    #[test]
    fn test_rebuilt_skips_a_named_path_the_filesystem_no_longer_holds() {
        let directory = fixture();
        let root = directory.path();
        let index = indexed(root, &TextFileInclusion::default());
        fs::write(root.join("src/transient.rs"), "pub fn transient() {}\n").expect("source");
        let changes = resolved(&index, root, &["src/transient.rs"]);
        fs::remove_file(root.join("src/transient.rs")).expect("the file leaves before the read");

        let next = index
            .rebuilt(&changes)
            .expect("a vanished path is not a refusal");
        assert_eq!(next.file_count(), index.file_count());
    }

    /// Builds the oversized case under a one-mebibyte per-file bound: the default `split`
    /// strategy holds a file up to the workspace byte bound, and writing a file past that
    /// bound would spend the test on half a gibibyte of disk writes.
    #[test]
    fn test_rebuilt_omits_a_newly_invalid_file_and_recovers_a_fixed_one() {
        let directory = fixture();
        let root = directory.path();
        let limits = WorkspaceIndexLimits::new(
            WORKSPACE_FILES_MAX_DEFAULT,
            1_048_576,
            WORKSPACE_BYTES_MAX_DEFAULT,
            WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT,
            READ_RESULTS_MAX_DEFAULT,
        )
        .expect("every bound is positive");
        let index = WorkspaceIndex::build(
            root,
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("the fixture workspace must index");
        assert!(index.warnings().is_empty());
        let lib_path = ProjectPath::new("src/lib.rs").expect("fixture path");

        // src/lib.rs turns invalid; the rebuild still lands, omitting it and warning.
        fs::write(root.join("src/lib.rs"), [0xff]).expect("corrupted source");
        let changes = resolved(&index, root, &["src/lib.rs"]);
        let corrupted = index
            .rebuilt(&changes)
            .expect("one invalid file must not fail the rebuild");
        assert!(
            corrupted.file(&lib_path).is_none(),
            "the corrupted file is dropped"
        );
        assert!(
            matches!(corrupted.warnings(), [warning]
                if warning_matches(warning, &lib_path, errors::index::workspace_invalid_source::SLUG)),
            "the rebuild carries a warning naming the corrupted file"
        );

        fs::write(root.join("src/lib.rs"), b"pub fn hidden() {}\0").expect("binary source");
        let changes = resolved(&corrupted, root, &["src/lib.rs"]);
        let binary = corrupted
            .rebuilt(&changes)
            .expect("one binary provider file must not fail rebuild");
        assert!(binary.file(&lib_path).is_none());
        assert!(binary.text_file(&lib_path).is_none());
        assert_eq!(
            binary.warnings(),
            [WorkspaceIndexWarning::BinarySource(lib_path.clone())]
        );
        assert!(
            binary
                .text_file(&ProjectPath::new("README.txt").expect("path"))
                .is_some(),
            "unrelated valid file remains indexed"
        );

        fs::write(
            root.join("src/lib.rs"),
            vec![b'x'; binary.limits.file_bytes_max() + 1],
        )
        .expect("oversized source");
        let changes = resolved(&binary, root, &["src/lib.rs"]);
        let oversized = binary
            .rebuilt(&changes)
            .expect("one oversized provider file must not fail rebuild");
        assert!(oversized.file(&lib_path).is_none());
        assert!(oversized.text_file(&lib_path).is_none());
        assert!(matches!(oversized.warnings(), [warning]
            if warning_matches(warning, &lib_path, errors::index::workspace_file_too_large::SLUG)));

        // Repairing the bytes and rebuilding again clears the warning and restores the file.
        fs::write(root.join("src/lib.rs"), "pub struct Rift;\n").expect("repaired source");
        let changes = resolved(&oversized, root, &["src/lib.rs"]);
        let repaired = oversized
            .rebuilt(&changes)
            .expect("the repaired file must rebuild");
        assert!(
            repaired.file(&lib_path).is_some(),
            "the repaired file is indexed again"
        );
        assert!(
            repaired.warnings().is_empty(),
            "the warning clears once the file is fixed"
        );
    }

    #[test]
    fn test_nested_ignore_file_narrows_only_its_own_directory() {
        // A pattern in a nested file addresses paths under that file's directory, so a
        // root-level file spelling the same name stays visible.
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("nested")).expect("fixture directory");
        fs::write(root.join("nested/.gitignore"), "hidden.rs\n").expect("fixture ignore file");
        fs::write(root.join("nested/hidden.rs"), "pub fn hidden() {}\n").expect("fixture source");
        fs::write(root.join("hidden.rs"), "pub fn visible() {}\n").expect("fixture source");

        let policy = WorkspaceSourcePolicy::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("the fixture policy must compile");
        let canonical = root.canonicalize().expect("canonical fixture root");
        assert!(
            !policy.visible(&canonical.join("nested/hidden.rs")),
            "the nested file excludes the path it names"
        );
        assert!(
            policy.visible(&canonical.join("hidden.rs")),
            "the same spelling above that directory stays visible"
        );
    }

    #[test]
    fn test_a_deeper_ignore_file_decides_over_a_shallower_one() {
        // Git lets a deeper file re-include what a shallower one excluded, and the policy
        // has to reach the same verdict as the walk that indexes the tree.
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("nested")).expect("fixture directory");
        fs::write(root.join(".gitignore"), "*.rs\n").expect("root ignore file");
        fs::write(root.join("nested/.gitignore"), "!kept.rs\n").expect("nested ignore file");
        fs::write(root.join("nested/kept.rs"), "pub fn kept() {}\n").expect("fixture source");
        fs::write(root.join("dropped.rs"), "pub fn dropped() {}\n").expect("fixture source");

        let index = WorkspaceIndex::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("the fixture workspace must index");
        let policy = WorkspaceSourcePolicy::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("the fixture policy must compile");
        let canonical = root.canonicalize().expect("canonical fixture root");

        assert_eq!(
            index
                .file(&ProjectPath::new("nested/kept.rs").expect("fixture path"))
                .is_some(),
            policy.visible(&canonical.join("nested/kept.rs")),
            "the walk and the policy must agree on a re-included path"
        );
        assert_eq!(
            index
                .file(&ProjectPath::new("dropped.rs").expect("fixture path"))
                .is_some(),
            policy.visible(&canonical.join("dropped.rs")),
            "the walk and the policy must agree on an excluded path"
        );
    }

    #[test]
    fn test_index_builds_composed_direct_workspace_reads() {
        let directory = fixture();
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("workspace index");
        assert_eq!(index.file_count(), 1);
        assert_eq!(index.composition().steps().len(), 3);
        let symbols = index.symbols("update", 5).expect("bounded symbol read");
        assert_eq!(symbols[0].symbol.qualified_name, "Rift::update");
        let source = index.source_matches("pub struct", 5).expect("lexical read");
        assert_eq!(source[0].1, 1);
        let path = ProjectPath::new("src/lib.rs").expect("fixture path");
        let file = index.file(&path).expect("indexed source");
        let provider = registry::provider_for_language(file.syntax().language())
            .expect("language has syntax provider");
        let complete = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: file.source(),
                },
                index.limits().syntax(),
            )
            .expect("complete syntax parse");
        let expected = complete
            .nodes_at(4)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            index.nodes(&path, 4).expect("node parse"),
            Some(expected),
            "reparsed nodes preserve complete provider rows and order"
        );
    }

    /// `README.txt` reaches the index only through the text lane; `text_matches` finds its
    /// content lines directly, the same way `source_matches` finds a syntax file's.
    #[test]
    fn test_index_text_matches_finds_content_lines_in_an_included_text_file() {
        let directory = fixture();
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("workspace index");
        let text = index.text_matches("ignored", 5).expect("lexical read");
        assert_eq!(text.len(), 1);
        assert_eq!(text[0].0.path().as_str(), "README.txt");
        assert_eq!(text[0].1, 1);
        assert_eq!(text[0].2, "ignored");
    }

    #[test]
    fn test_workspace_source_policy_matches_configuration_gitignore_and_hard_floor() {
        let directory = fixture();
        let watched_root = directory.path().join(".");
        fs::create_dir_all(directory.path().join("src/generated")).expect("generated directory");
        fs::create_dir(directory.path().join("target")).expect("target directory");
        fs::write(directory.path().join(".gitignore"), "src/ignored.rs\n").expect("ignore policy");
        let visibility = SourceVisibility::new(
            vec!["src/**".to_owned()],
            vec!["src/generated/**".to_owned()],
            true,
        );
        let policy = WorkspaceSourcePolicy::build(
            &watched_root,
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect("source policy");

        assert!(
            !policy.visible(std::path::Path::new("/nowhere/outside/the/root.rs")),
            "a path that does not normalize under the watched root is not visible"
        );
        assert!(policy.visible(&directory.path().join("src/lib.rs")));
        assert!(!policy.visible(&directory.path().join("src/ignored.rs")));
        assert!(!policy.visible(&directory.path().join("src/generated/code.rs")));
        assert!(!policy.visible(&directory.path().join("target/code.rs")));
        assert!(
            policy.visible(&directory.path().join("src/logo.png")),
            "visibility does not depend on a file extension"
        );
        assert!(!policy.visible(Path::new("outside.rs")));
        assert!(policy.may_include_descendant(&directory.path().join("src")));
        assert!(!policy.may_include_descendant(&directory.path().join("examples")));
        assert!(!policy.may_include_descendant(&directory.path().join("src/generated")));
        assert!(!policy.may_include_descendant(&directory.path().join("target")));
        let canonical_root = fs::canonicalize(directory.path()).expect("canonical workspace");
        assert!(policy.visible(&canonical_root.join("src/lib.rs")));
        assert!(policy.may_include_descendant(&canonical_root.join("src")));
    }

    #[test]
    fn test_workspace_source_policy_applies_same_visibility_to_all_files() {
        // `.gitignore` and `[source]` apply to provider files and baseline text alike.
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("docs/generated")).expect("directories");
        fs::write(directory.path().join(".gitignore"), "docs/ignored.mdx\n").expect("ignore file");
        fs::write(directory.path().join("docs/guide.mdx"), "guide").expect("guide");
        fs::write(directory.path().join("docs/ignored.mdx"), "ignored").expect("ignored");
        fs::write(directory.path().join("docs/generated/gen.mdx"), "generated").expect("generated");
        fs::write(directory.path().join("notes.txt"), "notes").expect("notes");
        fs::write(directory.path().join("logo.png"), "not text").expect("non-text");
        let visibility =
            SourceVisibility::new(Vec::new(), vec!["docs/generated/**".to_owned()], true);
        let policy = WorkspaceSourcePolicy::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect("source policy");

        assert!(policy.visible(&directory.path().join("docs/guide.mdx")));
        assert!(policy.visible(&directory.path().join("notes.txt")));
        assert!(
            !policy.visible(&directory.path().join("docs/ignored.mdx")),
            "gitignore must hide a text candidate exactly as it would a source one"
        );
        assert!(
            !policy.visible(&directory.path().join("docs/generated/gen.mdx")),
            "[source] exclude must hide a text candidate exactly as it would a source one"
        );
        assert!(
            policy.visible(&directory.path().join("logo.png")),
            "visibility does not depend on a file extension"
        );
    }

    #[test]
    fn test_nested_linked_worktree_is_excluded_from_walk_capture_and_events() {
        use rift_history::fixture::{commit_all, git, init};

        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        init(root);
        fs::write(root.join("lib.rs"), "pub fn main_beacon() {}\n").expect("main source");
        commit_all(root, "initial source");
        git(root, &["worktree", "add", "--detach", "linked", "HEAD"]);
        let independent = root.join("independent");
        fs::create_dir(&independent).expect("nested repository");
        init(&independent);
        fs::write(
            independent.join("lib.rs"),
            "pub fn independent_beacon() {}\n",
        )
        .expect("independent source");
        let origin = tempfile::tempdir().expect("submodule origin");
        init(origin.path());
        fs::write(
            origin.path().join("lib.rs"),
            "pub fn submodule_beacon() {}\n",
        )
        .expect("submodule source");
        commit_all(origin.path(), "submodule source");
        let origin_path = origin.path().to_str().expect("fixture path");
        git(
            root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                origin_path,
                "module",
            ],
        );
        let visibility = SourceVisibility::new(Vec::new(), Vec::new(), false)
            .with_force_include(vec!["linked/**".to_owned()]);
        let policy = WorkspaceSourcePolicy::build(
            root,
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect("workspace policy");
        let paths = policy.visible_paths().expect("visible source");
        assert!(
            paths
                .iter()
                .all(|path| !path.as_str().starts_with("linked/"))
        );
        assert!(
            paths
                .iter()
                .any(|path| path.as_str() == "independent/lib.rs")
        );
        assert!(paths.iter().any(|path| path.as_str() == "module/lib.rs"));
        assert!(!policy.visible(&root.join("linked/lib.rs")));
        assert!(!policy.may_include_descendant(&root.join("linked")));
        let index = WorkspaceIndex::build(
            root,
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect("workspace index");
        let capture = capture_digests(root, WorkspaceIndexLimits::default(), &visibility)
            .expect("workspace capture");
        assert_eq!(capture.fingerprint(), *index.fingerprint());
        assert!(
            capture
                .iter()
                .all(|(path, _)| !path.as_str().starts_with("linked/"))
        );
        assert!(has_symbol(&index, "independent_beacon"));
        assert!(has_symbol(&index, "submodule_beacon"));
        let linked_index = WorkspaceIndex::build(
            &root.join("linked"),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("linked worktree serves its own root");
        assert!(has_symbol(&linked_index, "main_beacon"));
    }

    /// Language selection answers only for a path visibility already accepted:
    /// one that does not normalize under the watched root, and one `[source]`
    /// excludes, both select nothing rather than reaching the language table.
    #[test]
    fn test_language_for_path_answers_none_outside_the_root_and_for_an_excluded_path() {
        let directory = fixture();
        fs::write(
            directory.path().join("excluded.rs"),
            "pub fn excluded() {}\n",
        )
        .expect("excluded source");
        let visibility = SourceVisibility::new(Vec::new(), vec!["excluded.rs".to_owned()], true);
        let policy = WorkspaceSourcePolicy::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect("source policy");
        let selected = policy
            .language_for_path(&directory.path().join("src/lib.rs"))
            .expect("lookup")
            .expect("a visible Rust path selects the shipped entry");
        assert_eq!(selected.identity(), "rust");
        assert!(
            policy
                .language_for_path(Path::new("outside.rs"))
                .expect("lookup")
                .is_none(),
            "a path that does not normalize under the watched root selects nothing"
        );
        assert!(
            policy
                .language_for_path(&directory.path().join("excluded.rs"))
                .expect("lookup")
                .is_none(),
            "a [source] exclude match selects nothing"
        );
    }

    /// A digest read answers for what the policy shows and stays silent for what
    /// it hides, so a hidden file never contributes workspace state.
    #[test]
    fn test_visible_digest_answers_none_for_a_path_the_policy_hides() {
        let directory = fixture();
        fs::write(
            directory.path().join("excluded.rs"),
            "pub fn excluded() {}\n",
        )
        .expect("excluded source");
        let visibility = SourceVisibility::new(Vec::new(), vec!["excluded.rs".to_owned()], true);
        let policy = WorkspaceSourcePolicy::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect("source policy");
        assert!(
            policy
                .visible_digest(&directory.path().join("src/lib.rs"))
                .expect("digest read")
                .is_some(),
            "a visible file answers with its digest"
        );
        assert!(
            policy
                .visible_digest(&directory.path().join("excluded.rs"))
                .expect("digest read")
                .is_none(),
            "a hidden file answers with nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_index_skips_symlinks_and_state_directories() {
        let directory = fixture();
        let outside = tempfile::tempdir().expect("outside directory");
        fs::write(outside.path().join("escape.rs"), "fn escaped() {}").expect("outside source");
        unix_fs::symlink(
            outside.path().join("escape.rs"),
            directory.path().join("src/escape.rs"),
        )
        .expect("source symlink");
        fs::create_dir(directory.path().join(".rift")).expect("state directory");
        fs::write(directory.path().join(".rift/hidden.rs"), "fn hidden() {}")
            .expect("state source");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("workspace index");
        assert!(index.symbols("escaped", 5).expect("symbol read").is_empty());
        assert!(index.symbols("hidden", 5).expect("symbol read").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_discover_skips_entries_that_are_neither_file_nor_directory() {
        let directory = fixture();
        let status = std::process::Command::new("mkfifo")
            .arg(directory.path().join("src/pipe.rs"))
            .status()
            .expect("mkfifo must run");
        assert!(status.success(), "mkfifo must create the named pipe");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("workspace index");
        assert_eq!(
            index.file_count(),
            1,
            "a named pipe is neither a directory nor a regular file and must be skipped"
        );
    }

    fn build_index(
        directory: &tempfile::TempDir,
        visibility: &SourceVisibility,
    ) -> Result<WorkspaceIndex, RiftError> {
        WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            visibility,
            &rift_core::TextFileInclusion::default(),
        )
    }

    fn has_symbol(index: &WorkspaceIndex, name: &str) -> bool {
        !index
            .symbols(name, 5)
            .expect("bounded symbol read")
            .is_empty()
    }

    #[test]
    fn test_force_include_reaches_gitignored_file_and_skips_already_indexed() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join(".gitignore"), "hidden.rs\n").expect("root gitignore");
        fs::write(directory.path().join("lib.rs"), "pub fn kept() {}\n").expect("kept source");
        fs::write(directory.path().join("hidden.rs"), "pub fn phantom() {}\n")
            .expect("hidden source");
        let index = build_index(&directory, &SourceVisibility::default()).expect("index");
        assert!(has_symbol(&index, "kept"));
        assert!(!has_symbol(&index, "phantom"));

        let extra = index
            .force_include_files(&["hidden.rs".to_owned()], 10)
            .expect("force_include walk");
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0].path().as_str(), "hidden.rs");
        assert!(
            extra[0]
                .syntax()
                .symbols()
                .iter()
                .any(|symbol| symbol.name == "phantom")
        );

        let indexed_only = index
            .force_include_files(&["lib.rs".to_owned()], 10)
            .expect("force_include of an already-indexed file");
        assert!(
            indexed_only.is_empty(),
            "force_include of an indexed file must not duplicate it"
        );
    }

    #[test]
    fn test_force_include_respects_hard_floor_and_bound() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(
            directory.path().join(".gitignore"),
            "a.rs\nb.rs\nfloor.rs\n",
        )
        .expect("root gitignore");
        fs::create_dir_all(directory.path().join(".git")).expect("git directory");
        fs::write(
            directory.path().join(".git/floor.rs"),
            "pub fn floor() {}\n",
        )
        .expect("floor source");
        fs::write(directory.path().join("a.rs"), "pub fn a() {}\n").expect("a source");
        fs::write(directory.path().join("b.rs"), "pub fn b() {}\n").expect("b source");
        let index = build_index(&directory, &SourceVisibility::default()).expect("index");

        let floor_reach = index
            .force_include_index(&[".git/**".to_owned()], 10)
            .expect("force_include walk");
        assert!(
            floor_reach.text_files().next().is_none(),
            "the hard floor must stay unreachable via force_include"
        );

        let one_provider = index
            .force_include_index(&["a.rs".to_owned()], 1)
            .expect("one provider path counts once");
        let a = ProjectPath::new("a.rs").expect("path");
        assert!(one_provider.file(&a).is_some());
        assert!(one_provider.text_file(&a).is_some());

        let bound_error = index
            .force_include_index(&["*.rs".to_owned()], 1)
            .expect_err("two matches must refuse a one-file bound");
        assert_eq!(
            bound_error.slug(),
            errors::index::workspace_too_many_files::SLUG
        );
    }

    /// Past `files_max`, the walk keeps counting the selector's matches without reading
    /// them, so the refusal names the first excess path and carries the bound and the
    /// match count as evidence.
    #[test]
    fn test_force_include_past_the_file_bound_carries_the_match_count_as_evidence() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join(".gitignore"), "*.rs\n").expect("root gitignore");
        for name in ["a.rs", "b.rs", "c.rs"] {
            fs::write(directory.path().join(name), "pub fn hidden() {}\n").expect("hidden source");
        }
        let index = build_index(&directory, &SourceVisibility::default()).expect("index");

        let error = index
            .force_include_index(&["*.rs".to_owned()], 1)
            .expect_err("three matches must refuse a one-file bound");
        assert_eq!(error.slug(), errors::index::workspace_too_many_files::SLUG);
        assert!(
            error_path(&error).is_some_and(|path| path.ends_with("b.rs")),
            "the refusal names the first path past the bound: {:?}",
            error_path(&error)
        );
        assert_eq!(
            limit_evidence(&error),
            Some(("paths.force_include".to_owned(), 3, 1))
        );
    }

    #[test]
    fn test_force_include_reports_too_deep_like_the_ordinary_scan() {
        // `outer/` is gitignored, so the ordinary scan never walks deep enough to see `inner/`
        // and `WorkspaceIndex::build` succeeds under a depth bound of 1. `force_include`
        // ignores `.gitignore`, so its own walk reaches `inner/` and must report the same
        // `TooDeep` bound the ordinary scan would have, rather than silently stopping short.
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join(".gitignore"), "outer/\n").expect("root gitignore");
        fs::write(directory.path().join("kept.rs"), "pub fn kept() {}\n").expect("kept source");
        fs::create_dir_all(directory.path().join("outer/inner")).expect("nested directories");
        fs::write(
            directory.path().join("outer/inner/deep.rs"),
            "pub fn deep() {}\n",
        )
        .expect("nested source");
        let limits = WorkspaceIndexLimits::new(5, 1_000, 2_000, 1, 5).expect("positive limits");
        let shallow = WorkspaceIndex::build(
            directory.path(),
            limits,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("index bounded to depth 1, with the deep directory hidden by .gitignore");
        let error = shallow
            .force_include_files(&["outer/**".to_owned()], 10)
            .expect_err("a directory past the depth bound must refuse");
        assert_eq!(error.slug(), errors::index::workspace_too_deep::SLUG);
    }

    /// The force-include walk carries syntax facts only. A matched path the text
    /// lane claims is skipped there, so provider parsing stays separate from file
    /// content.
    #[test]
    fn test_force_include_skips_a_matched_path_the_text_lane_claims() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("notes.txt"), "prose\n").expect("prose");
        let index = build_index(&directory, &SourceVisibility::default()).expect("index");
        let extra = index
            .force_include_files(&["notes.txt".to_owned()], 10)
            .expect("force_include walk");
        assert!(
            extra.is_empty(),
            "a text-lane path carries no syntax facts to force-include"
        );
    }

    #[test]
    fn test_force_include_of_empty_list_returns_no_files() {
        let directory = fixture();
        let index = build_index(&directory, &SourceVisibility::default()).expect("index");
        let extra = index
            .force_include_files(&[], 10)
            .expect("an empty force_include list must not walk for matches");
        assert!(extra.is_empty());
    }

    /// One visible source file and one note below a directory `.gitignore` hides.
    fn hidden_notes_fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("src")).expect("source directory");
        fs::create_dir_all(directory.path().join("notes")).expect("notes directory");
        fs::write(directory.path().join(".gitignore"), "notes/\n").expect("ignore policy");
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n").expect("source");
        fs::write(directory.path().join("notes/plan.txt"), "plan\n").expect("note");
        directory
    }

    /// Whether the built index holds `path` in its baseline text catalog.
    fn holds_text(index: &WorkspaceIndex, path: &str) -> bool {
        let path = ProjectPath::new(path).expect("fixture path");
        index.text_file(&path).is_some()
    }

    fn built(root: &Path, visibility: &SourceVisibility) -> WorkspaceIndex {
        WorkspaceIndex::build(
            root,
            WorkspaceIndexLimits::default(),
            visibility,
            &TextFileInclusion::default(),
        )
        .expect("index")
    }

    #[test]
    fn test_source_force_include_indexes_a_gitignored_path() {
        let directory = hidden_notes_fixture();
        let visibility =
            SourceVisibility::default().with_force_include(vec!["notes/**".to_owned()]);
        let index = built(directory.path(), &visibility);
        assert!(
            holds_text(&index, "notes/plan.txt"),
            "force_include must reach a path the workspace's own .gitignore hides"
        );
    }

    #[test]
    fn test_a_gitignored_path_stays_hidden_without_force_include() {
        let directory = hidden_notes_fixture();
        let index = built(directory.path(), &SourceVisibility::default());
        assert!(
            !holds_text(&index, "notes/plan.txt"),
            "the same tree without force_include indexes exactly what it indexed before"
        );
    }

    #[test]
    fn test_source_exclude_wins_over_force_include() {
        let directory = hidden_notes_fixture();
        let visibility = SourceVisibility::new(Vec::new(), vec!["notes/**".to_owned()], true)
            .with_force_include(vec!["notes/**".to_owned()]);
        let index = built(directory.path(), &visibility);
        assert!(
            !holds_text(&index, "notes/plan.txt"),
            "both lists are the operator's own statement, and the narrower one decides"
        );
    }

    #[test]
    fn test_source_force_include_needs_no_include_match() {
        let directory = hidden_notes_fixture();
        let visibility = SourceVisibility::new(vec!["src/**".to_owned()], Vec::new(), true)
            .with_force_include(vec!["notes/**".to_owned()]);
        let index = built(directory.path(), &visibility);
        assert!(
            holds_text(&index, "notes/plan.txt"),
            "a force_include match is kept although include names another subtree"
        );
        assert_eq!(index.file_count(), 1, "the included source stays indexed");
    }

    #[test]
    fn test_source_force_include_leaves_other_gitignored_paths_hidden() {
        let directory = hidden_notes_fixture();
        fs::create_dir_all(directory.path().join("cache")).expect("cache directory");
        fs::write(directory.path().join(".gitignore"), "notes/\ncache/\n").expect("ignore policy");
        fs::write(directory.path().join("cache/entry.txt"), "entry\n").expect("cache entry");
        let visibility =
            SourceVisibility::default().with_force_include(vec!["notes/**".to_owned()]);
        let index = built(directory.path(), &visibility);
        assert!(holds_text(&index, "notes/plan.txt"));
        assert!(
            !holds_text(&index, "cache/entry.txt"),
            "a gitignored path outside every force_include glob stays hidden"
        );
    }

    #[test]
    fn test_source_force_include_never_reaches_the_hard_floor() {
        let directory = hidden_notes_fixture();
        fs::create_dir_all(directory.path().join("target")).expect("target directory");
        fs::write(directory.path().join("target/note.txt"), "built\n").expect("built artifact");
        let visibility = SourceVisibility::default()
            .with_force_include(vec!["target/**".to_owned(), "notes/**".to_owned()]);
        let index = built(directory.path(), &visibility);
        assert!(holds_text(&index, "notes/plan.txt"));
        assert!(
            !holds_text(&index, "target/note.txt"),
            "the hard floor stays unreachable whatever the [source] table says"
        );
    }

    #[test]
    fn test_source_force_include_records_a_visible_path_once() {
        let directory = hidden_notes_fixture();
        let visibility = SourceVisibility::default()
            .with_force_include(vec!["notes/**".to_owned(), "src/**".to_owned()]);
        let index = built(directory.path(), &visibility);
        assert_eq!(
            index.file_count(),
            1,
            "a path the gitignore-respecting walk already recorded is not recorded twice"
        );
        assert!(holds_text(&index, "notes/plan.txt"));
    }

    #[test]
    fn test_source_force_include_shares_the_files_bound() {
        let directory = hidden_notes_fixture();
        let visibility =
            SourceVisibility::default().with_force_include(vec!["notes/**".to_owned()]);
        let limits = WorkspaceIndexLimits {
            files_max: 1,
            ..WorkspaceIndexLimits::default()
        };
        let error = WorkspaceIndex::build(
            directory.path(),
            limits,
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect_err("the second walk spends the same budget as the first");
        assert_eq!(error.slug(), errors::index::workspace_too_many_files::SLUG);
        let (_, observed, maximum) = limit_evidence(&error).expect("limit evidence");
        assert_eq!((maximum, observed), (1, 2));
    }

    #[test]
    fn test_source_force_include_invalid_glob_refuses() {
        let directory = hidden_notes_fixture();
        let visibility = SourceVisibility::default().with_force_include(vec!["[".to_owned()]);
        let error = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect_err("an unclosed character class must be refused");
        assert_eq!(error.slug(), errors::analysis::source_pattern_invalid::SLUG);
    }

    #[test]
    fn test_workspace_source_policy_force_include_survives_gitignore() {
        let directory = hidden_notes_fixture();
        let visibility = SourceVisibility::new(vec!["src/**".to_owned()], Vec::new(), true)
            .with_force_include(vec!["notes/**".to_owned()]);
        let policy = WorkspaceSourcePolicy::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect("source policy");

        assert!(
            policy.visible(&directory.path().join("notes/plan.txt")),
            "a filesystem event on a force-included path reaches the index"
        );
        assert!(
            policy.may_include_descendant(&directory.path().join("notes")),
            "the walk descends toward a force_include glob although include names another subtree"
        );
        assert!(policy.visible(&directory.path().join("src/lib.rs")));
        assert!(!policy.visible(&directory.path().join("target/note.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn test_force_include_skips_entries_that_are_neither_file_nor_directory() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let status = std::process::Command::new("mkfifo")
            .arg(directory.path().join("pipe.rs"))
            .status()
            .expect("mkfifo must run");
        assert!(status.success(), "mkfifo must create the named pipe");
        let index = build_index(&directory, &SourceVisibility::default()).expect("index");
        let extra = index
            .force_include_files(&["*.rs".to_owned()], 10)
            .expect("force_include walk");
        assert!(
            extra.is_empty(),
            "a named pipe is neither a directory nor a regular file and must be skipped"
        );
    }

    #[test]
    fn test_force_include_invalid_glob_refuses() {
        let directory = fixture();
        let index = build_index(&directory, &SourceVisibility::default()).expect("index");
        let error = index
            .force_include_files(&["[".to_owned()], 10)
            .expect_err("an unclosed character class must be refused");
        assert_eq!(error.slug(), errors::analysis::source_pattern_invalid::SLUG);
    }

    #[test]
    fn test_gitignore_chain_hides_matching_files_including_nested() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("src/generated")).expect("fixture directories");
        fs::write(directory.path().join("src/lib.rs"), "pub fn kept() {}\n").expect("kept source");
        fs::write(
            directory.path().join("src/generated/gen.rs"),
            "pub fn generated() {}\n",
        )
        .expect("generated source");
        fs::write(directory.path().join("src/generated/.gitignore"), "*.rs\n")
            .expect("nested gitignore");

        let index = build_index(&directory, &SourceVisibility::default()).expect("index");
        assert!(has_symbol(&index, "kept"));
        assert!(!has_symbol(&index, "generated"));
    }

    #[test]
    fn test_respect_gitignore_toggle_includes_or_hides_matching_files() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("vendor")).expect("fixture directories");
        fs::write(directory.path().join(".gitignore"), "vendor/\n").expect("root gitignore");
        fs::write(
            directory.path().join("vendor/dep.rs"),
            "pub fn vendored() {}\n",
        )
        .expect("vendored source");

        let respecting = build_index(&directory, &SourceVisibility::default()).expect("index");
        assert!(!has_symbol(&respecting, "vendored"));

        let ignoring = SourceVisibility::new(Vec::new(), Vec::new(), false);
        let index = build_index(&directory, &ignoring).expect("index");
        assert!(has_symbol(&index, "vendored"));
    }

    #[test]
    fn test_include_narrows_visibility_to_matching_files() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("src")).expect("fixture directories");
        fs::write(directory.path().join("src/lib.rs"), "pub fn kept() {}\n").expect("kept source");
        fs::write(directory.path().join("other.rs"), "pub fn other() {}\n").expect("other source");

        let visibility = SourceVisibility::new(vec!["src/**".to_owned()], Vec::new(), true);
        let index = build_index(&directory, &visibility).expect("index");
        assert_eq!(index.file_count(), 1);
        assert!(has_symbol(&index, "kept"));
        assert!(!has_symbol(&index, "other"));
    }

    #[test]
    fn test_exclude_drops_matching_files_even_when_included() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("src/generated")).expect("fixture directories");
        fs::write(directory.path().join("src/lib.rs"), "pub fn kept() {}\n").expect("kept source");
        fs::write(
            directory.path().join("src/generated/gen.rs"),
            "pub fn generated() {}\n",
        )
        .expect("generated source");

        let visibility = SourceVisibility::new(
            vec!["src/**".to_owned()],
            vec!["src/generated/**".to_owned()],
            true,
        );
        let index = build_index(&directory, &visibility).expect("index");
        assert!(has_symbol(&index, "kept"));
        assert!(!has_symbol(&index, "generated"));
    }

    #[test]
    fn test_invalid_include_glob_reports_source_pattern_invalid() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("lib.rs"), "pub fn kept() {}\n").expect("kept source");

        let visibility = SourceVisibility::new(vec!["[".to_owned()], Vec::new(), true);
        let error = build_index(&directory, &visibility).expect_err("unclosed glob class");
        assert_eq!(error.slug(), errors::analysis::source_pattern_invalid::SLUG);
    }

    #[test]
    fn test_invalid_exclude_glob_reports_source_pattern_invalid() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("lib.rs"), "pub fn kept() {}\n").expect("kept source");

        let visibility = SourceVisibility::new(Vec::new(), vec!["[".to_owned()], true);
        let error = build_index(&directory, &visibility).expect_err("unclosed glob class");
        assert_eq!(error.slug(), errors::analysis::source_pattern_invalid::SLUG);
    }

    #[test]
    fn test_hard_floor_hides_git_rift_and_target_regardless_of_config() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        for name in [".git", ".rift", "target"] {
            fs::create_dir_all(directory.path().join(name)).expect("floor directory");
            fs::write(
                directory.path().join(name).join("floor.rs"),
                "pub fn floor() {}\n",
            )
            .expect("floor source");
        }

        // respect_gitignore is off and the hard-floor directories are force-listed in
        // include: the floor must still win.
        let visibility = SourceVisibility::new(
            vec![
                ".git/**".to_owned(),
                ".rift/**".to_owned(),
                "target/**".to_owned(),
            ],
            Vec::new(),
            false,
        );
        let index = build_index(&directory, &visibility).expect("index");
        assert!(!has_symbol(&index, "floor"));
        assert_eq!(index.file_count(), 0);
    }

    #[test]
    fn test_dotfiles_stay_visible() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join(".hidden.rs"), "pub fn dotfile() {}\n")
            .expect("dotfile source");
        fs::create_dir_all(directory.path().join(".config")).expect("dot directory");
        fs::write(
            directory.path().join(".config/mod.rs"),
            "pub fn dotdir() {}\n",
        )
        .expect("dotdir source");

        let index = build_index(&directory, &SourceVisibility::default()).expect("index");
        assert!(has_symbol(&index, "dotfile"));
        assert!(has_symbol(&index, "dotdir"));
    }

    #[test]
    fn test_index_enforces_file_workspace_depth_and_result_bounds() {
        let directory = fixture();
        let bounded = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::new(2, 4, 100, 4, 5).expect("positive limits"),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("oversized files are omitted");
        assert_eq!(bounded.warnings().len(), 2);
        assert!(
            bounded
                .warnings()
                .iter()
                .all(|warning| matches!(warning, WorkspaceIndexWarning::FileTooLarge { .. }))
        );

        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("workspace index");
        assert_eq!(
            index
                .symbols("Rift", index.limits.results_max() + 1)
                .expect_err("result bound")
                .slug(),
            errors::index::workspace_result_limit::SLUG,
        );
        assert_eq!(
            index
                .source_matches("Rift", 0)
                .expect_err("zero result bound")
                .slug(),
            errors::index::workspace_result_limit::SLUG,
        );
    }

    #[test]
    fn test_index_enforces_scan_bounds_and_root_contract() {
        assert_eq!(
            WorkspaceIndexLimits::new(0, 1, 1, 1, 1)
                .expect_err("zero bound")
                .slug(),
            errors::index::workspace_zero_limit::SLUG,
        );

        let missing = PathBuf::from("missing-rift-workspace");
        let missing_error = WorkspaceIndex::build(
            &missing,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect_err("missing root");
        assert_eq!(
            missing_error.slug(),
            errors::index::workspace_invalid_root::SLUG
        );
        assert_eq!(error_path(&missing_error), Some(missing.clone()));
        assert!(std::error::Error::source(&missing_error).is_some());

        let directory = fixture();
        let file_root = directory.path().join("src/lib.rs");
        assert_eq!(
            WorkspaceIndex::build(
                &file_root,
                WorkspaceIndexLimits::default(),
                &SourceVisibility::default(),
                &rift_core::TextFileInclusion::default(),
            )
            .expect_err("file root")
            .slug(),
            errors::index::workspace_invalid_root::SLUG,
        );

        fs::write(directory.path().join("src/other.rs"), "fn other() {}").expect("second source");
        assert_eq!(
            WorkspaceIndex::build(
                directory.path(),
                WorkspaceIndexLimits::new(1, 1_000, 2_000, 4, 5).expect("limits"),
                &SourceVisibility::default(),
                &rift_core::TextFileInclusion::default(),
            )
            .expect_err("file count bound")
            .slug(),
            errors::index::workspace_too_many_files::SLUG,
        );
        assert_eq!(
            WorkspaceIndex::build(
                directory.path(),
                WorkspaceIndexLimits::new(5, 1_000, 8, 4, 5).expect("limits"),
                &SourceVisibility::default(),
                &rift_core::TextFileInclusion::default(),
            )
            .expect_err("workspace byte bound")
            .slug(),
            errors::index::workspace_workspace_too_large::SLUG,
        );

        fs::create_dir(directory.path().join("src/nested")).expect("nested directory");
        fs::write(directory.path().join("src/nested/deep.rs"), "fn deep() {}")
            .expect("nested source");
        assert_eq!(
            WorkspaceIndex::build(
                directory.path(),
                WorkspaceIndexLimits::new(5, 1_000, 2_000, 1, 5).expect("limits"),
                &SourceVisibility::default(),
                &rift_core::TextFileInclusion::default(),
            )
            .expect_err("depth bound")
            .slug(),
            errors::index::workspace_too_deep::SLUG,
        );
    }

    #[test]
    fn test_index_queries_cover_rank_and_early_limit_paths() {
        let directory = fixture();
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("workspace index");
        assert_eq!(
            index.root(),
            fs::canonicalize(directory.path()).expect("root")
        );

        let exact = index.symbols("Rift::update", 5).expect("qualified match");
        assert_eq!(exact[0].symbol.qualified_name, "Rift::update");
        assert_eq!(exact[0].rank, IdentifierMatchClass::QualifiedExact);
        assert_eq!(
            index.symbols("update", 5).expect("name match")[0].rank,
            IdentifierMatchClass::NameExact
        );
        assert_eq!(
            index.symbols("upd", 5).expect("prefix match")[0].rank,
            IdentifierMatchClass::NamePrefix
        );
        assert_eq!(
            index.symbols("pda", 5).expect("substring match")[0].rank,
            IdentifierMatchClass::Substring
        );
        assert_eq!(
            index
                .source_matches("pub", 1)
                .expect("early bounded source match")
                .len(),
            1,
        );
        let missing = ProjectPath::new("src/missing.rs").expect("missing path");
        assert!(index.file(&missing).is_none());
        assert!(
            index
                .nodes(&missing, 0)
                .expect("missing path has no node parse")
                .is_none()
        );
    }

    #[test]
    fn assembled_symbol_requires_normalized_record_and_portable_facts() {
        let directory = fixture();
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("workspace index");
        let matched = index
            .symbols("update", 1)
            .expect("symbol query")
            .into_iter()
            .next()
            .expect("update symbol");
        let readable = index
            .assembled_symbol(matched)
            .expect("normalized readable symbol");
        assert_eq!(
            readable.identity().map(rift_core::SymbolId::as_str),
            Some("rift://symbol/rust/src/lib.rs/Rift::update")
        );
        assert_eq!(readable.facts().name(), "update");
        assert!(!readable.assembled().contributions().is_empty());

        let other_directory = tempfile::tempdir().expect("other workspace");
        fs::write(
            other_directory.path().join("other.rs"),
            "pub fn foreign() {}\n",
        )
        .expect("other source");
        let other = WorkspaceIndex::build(
            other_directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("other index");
        let foreign = other
            .symbols("foreign", 1)
            .expect("foreign query")
            .into_iter()
            .next()
            .expect("foreign symbol");
        let error = index
            .assembled_symbol(foreign)
            .expect_err("foreign normalized record must be absent");
        assert_eq!(error.slug(), errors::index::workspace_provider::SLUG);
        let source =
            std::error::Error::source(&error).expect("provider failure must retain missing record");
        assert!(
            source.to_string().contains("foreign"),
            "missing record must name identity: {source}"
        );
    }

    #[test]
    fn test_error_display_appends_offending_path() {
        let error = errors::index::workspace_file_too_large()
            .path(Path::new("src/big.rs"))
            .maximum(64_usize)
            .error();
        assert_eq!(
            error.to_string(),
            "source file exceeds its accepted byte limit of 64: path src/big.rs; \
             reduce source file size below 64 bytes and retry"
        );
    }

    #[test]
    fn test_component_identity_failure_surfaces_as_composition_error() {
        let error = component::<(), WorkspaceFiles>("").expect_err("empty component id");
        assert_eq!(error.slug(), errors::index::workspace_composition::SLUG);
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn test_read_file_classifies_syntax_and_non_nfc_path_failures() {
        let directory = fixture();
        let strict_limits = WorkspaceIndexLimits {
            syntax: SyntaxLimits::new(1, 1, 1).expect("positive bounds"),
            ..WorkspaceIndexLimits::default()
        };
        let mut bytes = 0;
        let parser = RustSyntaxProvider::default();
        let source_path = directory.path().join("src/lib.rs");
        let syntax_error = read_file(
            directory.path(),
            &source_path,
            &parser,
            strict_limits,
            &mut bytes,
        )
        .expect_err("syntax byte bound");
        assert_eq!(syntax_error.slug(), errors::index::workspace_syntax::SLUG);
        assert_eq!(error_path(&syntax_error), Some(source_path.clone()));
        assert!(std::error::Error::source(&syntax_error).is_some());
        assert_eq!(
            source_rift_error(&syntax_error).map(RiftError::slug),
            Some(errors::syntax::source_too_large::SLUG),
            "a syntax failure must keep the underlying syntax classification"
        );

        let decomposed = directory.path().join("src/cafe\u{301}.rs");
        fs::write(&decomposed, "fn accent() {}").expect("decomposed source");
        let limits = WorkspaceIndexLimits::default();
        let path_error = read_file(directory.path(), &decomposed, &parser, limits, &mut bytes)
            .expect_err("non-NFC project path");
        assert_eq!(
            path_error.slug(),
            errors::index::workspace_invalid_path::SLUG
        );
        assert!(std::error::Error::source(&path_error).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_discover_classifies_unreadable_and_unsearchable_directories() {
        use std::os::unix::fs::PermissionsExt;

        let directory = fixture();
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let locked = root.join("locked");
        fs::create_dir(&locked).expect("locked directory");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("remove read");
        let unreadable = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect_err("unreadable directory");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restore read");
        assert_eq!(unreadable.slug(), errors::index::workspace_filesystem::SLUG);
        assert_eq!(error_path(&unreadable), Some(locked.clone()));

        let unsearchable = root.join("unsearchable");
        fs::create_dir(&unsearchable).expect("unsearchable directory");
        fs::write(unsearchable.join("entry.rs"), "fn entry() {}").expect("entry source");
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o444))
            .expect("remove search");
        let stat_error = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect_err("unsearchable directory");
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o755))
            .expect("restore search");
        assert_eq!(stat_error.slug(), errors::index::workspace_filesystem::SLUG);
        assert_eq!(error_path(&stat_error), Some(unsearchable.join("entry.rs")));
    }

    #[test]
    fn test_read_file_classifies_source_path_and_filesystem_failures() {
        let directory = fixture();
        let parser = RustSyntaxProvider::default();
        let limits = WorkspaceIndexLimits::default();
        let mut bytes = 0;
        let missing = directory.path().join("missing.rs");
        assert_eq!(
            read_file(directory.path(), &missing, &parser, limits, &mut bytes)
                .expect_err("missing source")
                .slug(),
            errors::index::workspace_filesystem::SLUG,
        );

        let outside = tempfile::tempdir().expect("outside directory");
        let outside_file = outside.path().join("outside.rs");
        fs::write(&outside_file, "fn outside() {}").expect("outside source");
        assert_eq!(
            read_file(directory.path(), &outside_file, &parser, limits, &mut bytes,)
                .expect_err("outside project path")
                .slug(),
            errors::index::workspace_invalid_path::SLUG,
        );

        let invalid = directory.path().join("src/invalid.rs");
        fs::write(&invalid, [0xff]).expect("invalid UTF-8");
        assert_eq!(
            read_file(directory.path(), &invalid, &parser, limits, &mut bytes)
                .expect_err("the file's own read refuses rather than returning empty content")
                .slug(),
            errors::index::workspace_invalid_source::SLUG,
        );

        let mut overflow = usize::MAX;
        assert_eq!(
            read_file(
                directory.path(),
                &directory.path().join("src/lib.rs"),
                &parser,
                limits,
                &mut overflow,
            )
            .expect_err("workspace byte overflow")
            .slug(),
            errors::index::workspace_workspace_too_large::SLUG,
        );
    }

    /// Builds a [`DiscoveredPaths`] with `source` classified as source and no text paths, for
    /// tests exercising [`capture_paths`] directly.
    fn source_only(source: Vec<PathBuf>) -> DiscoveredPaths {
        let provider = registry::provider_for_language(&rift_protocol::read::Language {
            name: "rust".to_owned(),
            dialect: None,
        })
        .expect("the shipped Rust provider");
        DiscoveredPaths {
            source: source.into_iter().map(|path| (path, provider)).collect(),
            text: Vec::new(),
            lockfiles: Vec::new(),
        }
    }

    #[test]
    fn file_byte_reads_stop_after_the_oversize_marker() {
        let limits = WorkspaceIndexLimits::new(5, 4, 100, 4, 5).expect("limits");
        let mut reader = std::io::Cursor::new(b"123456789".as_slice());
        let bytes = super::read_file_bytes(&mut reader, Path::new("large.rs"), limits)
            .expect("the bounded prefix reads");
        assert_eq!(
            bytes, b"12345",
            "one excess byte proves the file is oversized"
        );
        assert_eq!(reader.position(), 5, "remaining bytes must not be consumed");

        let mut empty = std::io::empty();
        assert!(
            super::read_file_bytes(&mut empty, Path::new("empty.rs"), limits)
                .expect("empty file reads")
                .is_empty()
        );
    }

    #[test]
    fn file_byte_reads_preserve_errors_within_the_bound() {
        struct FailedReader;

        impl std::io::Read for FailedReader {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("capture read failed"))
            }
        }

        let limits = WorkspaceIndexLimits::new(5, 4, 100, 4, 5).expect("limits");
        let error = super::read_file_bytes(FailedReader, Path::new("failed.rs"), limits)
            .expect_err("a failed read must not become an omitted file");
        assert_eq!(error.slug(), errors::index::workspace_filesystem::SLUG);
        assert!(error.to_string().contains("failed.rs"), "{error}");
        assert!(
            rift_error::causes(&error)
                .iter()
                .any(|cause| cause.contains("capture read failed"))
        );
    }

    /// Captures `paths` below `root` with nothing recorded to reuse.
    fn captured_paths(
        root: &Path,
        paths: &DiscoveredPaths,
        limits: WorkspaceIndexLimits,
    ) -> Result<WorkspaceDigests, RiftError> {
        capture_paths(root, paths, limits, &LastCapture::default()).map(|(digests, _)| digests)
    }

    #[test]
    fn test_capture_paths_counts_reused_lengths_against_the_workspace_bound() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let limits = WorkspaceIndexLimits::new(5, 8, 10, 4, 5).expect("limits");
        let first = root.join("first.rs");
        let oversized = root.join("oversized.rs");
        fs::write(&first, b"123456").expect("first source");
        fs::write(&oversized, b"123456789").expect("oversized source");
        let both = source_only(vec![first.clone(), oversized.clone()]);
        #[cfg(unix)]
        if crate::capture::supports_stat_reuse(&root) {
            advance_capture_clock(&root, &[first.clone(), oversized.clone()]);
        }
        let (digests, last) =
            capture_paths(&root, &both, limits, &LastCapture::default()).expect("first fits");
        let alone =
            captured_paths(&root, &source_only(vec![first.clone()]), limits).expect("first alone");
        assert_eq!(digests.fingerprint(), alone.fingerprint());
        let (_, next) = capture_paths(&root, &both, limits, &last).expect("nothing moved");
        assert_eq!(
            next.read_paths(),
            if cfg!(unix) && crate::capture::supports_stat_reuse(&root) {
                0
            } else {
                2
            },
            "the left-out file stays omitted; supported filesystems reuse its recorded length"
        );
        let second = root.join("second.rs");
        fs::write(&second, b"123456").expect("second source");
        let all = source_only(vec![first, oversized, second]);
        let error = capture_paths(&root, &all, limits, &next).expect_err("workspace bound");
        assert_eq!(
            error.slug(),
            errors::index::workspace_workspace_too_large::SLUG,
            "a reused length still counts against workspace_bytes_max"
        );
    }

    #[test]
    fn test_capture_paths_preserves_bound_and_path_failures() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let limits = WorkspaceIndexLimits::new(5, 8, 10, 4, 5).expect("limits");

        let missing = root.join("missing.rs");
        let error =
            captured_paths(&root, &source_only(vec![missing]), limits).expect_err("missing source");
        assert_eq!(
            error.slug(),
            errors::index::workspace_changed_during_capture::SLUG
        );
        assert!(
            !rift_error::causes(&error).is_empty(),
            "the missing path keeps its operating-system cause"
        );

        let oversized = root.join("oversized.rs");
        fs::write(&oversized, b"123456789").expect("oversized source");
        let digests = captured_paths(&root, &source_only(vec![oversized]), limits)
            .expect("oversized source is omitted");
        assert!(digests.is_empty());

        let first = root.join("first.rs");
        let second = root.join("second.rs");
        fs::write(&first, b"123456").expect("first source");
        fs::write(&second, b"123456").expect("second source");
        let error = captured_paths(&root, &source_only(vec![first, second]), limits)
            .expect_err("workspace bound");
        assert_eq!(
            error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );

        let outside = tempfile::NamedTempFile::new().expect("outside source");
        fs::write(outside.path(), b"fn x(){}").expect("outside bytes");
        let error = captured_paths(
            &root,
            &source_only(vec![outside.path().to_path_buf()]),
            limits,
        )
        .expect_err("outside path");
        assert_eq!(error.slug(), errors::index::workspace_invalid_path::SLUG);

        // Unlike every other case above, invalid UTF-8 does not fail the capture: the file
        // is omitted from the digest set instead, matching what a build omits from the
        // index over the same tree.
        let invalid = root.join("invalid.rs");
        fs::write(&invalid, [0xff]).expect("invalid source");
        let digests = captured_paths(&root, &source_only(vec![invalid]), limits)
            .expect("invalid UTF-8 is omitted rather than failing the capture");
        assert!(digests.is_empty(), "the invalid file contributes no digest");
    }

    #[test]
    fn test_capture_paths_applies_per_file_bound_to_every_file_class() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        // file_bytes_max is 4. Every file class shares this bound.
        let limits = WorkspaceIndexLimits::new(5, 4, 1_000, 4, 5).expect("limits");
        let big_text = root.join("big.md");
        fs::write(&big_text, b"much larger than four bytes").expect("big text file");
        let paths = DiscoveredPaths {
            source: Vec::new(),
            text: vec![big_text],
            lockfiles: Vec::new(),
        };
        let digests = captured_paths(&root, &paths, limits)
            .expect("a text file over file_bytes_max is omitted");
        assert!(digests.is_empty());

        let tight = WorkspaceIndexLimits::new(5, 4, 10, 4, 5).expect("limits");
        let over_workspace = root.join("over.md");
        fs::write(&over_workspace, b"still more than ten bytes total").expect("oversized text");
        let paths = DiscoveredPaths {
            source: Vec::new(),
            text: vec![over_workspace],
            lockfiles: Vec::new(),
        };
        let digests =
            captured_paths(&root, &paths, tight).expect("the per-file bound applies first");
        assert!(digests.is_empty());
    }

    #[test]
    fn test_capture_paths_text_class_omits_invalid_utf8_rather_than_refusing() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let limits = WorkspaceIndexLimits::default();
        let invalid = root.join("invalid.md");
        fs::write(&invalid, [0xff, 0xfe]).expect("invalid text bytes");
        let paths = DiscoveredPaths {
            source: Vec::new(),
            text: vec![invalid],
            lockfiles: Vec::new(),
        };
        let digests = captured_paths(&root, &paths, limits)
            .expect("invalid UTF-8 text is omitted rather than failing the capture");
        assert!(
            digests.is_empty(),
            "the invalid text file contributes no digest"
        );
    }

    #[test]
    fn test_descendant_inclusion_refuses_paths_outside_root() {
        let directory = fixture();
        let policy = WorkspaceSourcePolicy::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect("source policy");
        assert!(!policy.may_include_descendant(Path::new("/rift-elsewhere")));
    }

    #[test]
    fn test_hard_floor_refuses_event_paths_outside_root() {
        assert!(!hard_floor_includes_path(
            Path::new("/rift-workspace"),
            Path::new("/rift-elsewhere/lib.rs")
        ));
    }

    #[test]
    fn test_gitignore_files_beyond_the_file_bound_are_refused() {
        let directory = fixture();
        fs::write(directory.path().join(".gitignore"), "target\n").expect("root ignore");
        fs::write(directory.path().join("src/.gitignore"), "generated\n").expect("nested ignore");
        let tight = WorkspaceIndexLimits::new(1, 4_096, 65_536, 8, 10).expect("bounds");
        let error = WorkspaceSourcePolicy::build(
            directory.path(),
            tight,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect_err("second ignore file must breach the file bound");
        assert_eq!(error.slug(), errors::index::workspace_too_many_files::SLUG);
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_gitignore_is_a_filesystem_refusal() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = fixture();
        let ignore = directory.path().join(".gitignore");
        fs::write(&ignore, "target\n").expect("ignore fixture");
        fs::set_permissions(&ignore, fs::Permissions::from_mode(0o000)).expect("revoke read");
        let error = WorkspaceSourcePolicy::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect_err("unreadable ignore file must be refused");
        fs::set_permissions(&ignore, fs::Permissions::from_mode(0o644)).expect("restore read");
        assert_eq!(error.slug(), errors::index::workspace_filesystem::SLUG);
    }

    #[test]
    fn test_build_applies_visibility_once_to_baseline_catalog() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("docs/generated")).expect("directories");
        fs::write(directory.path().join(".gitignore"), "docs/ignored.md\n").expect("ignore file");
        fs::write(directory.path().join("docs/guide.md"), "guide body").expect("guide");
        fs::write(directory.path().join("docs/notes.mdx"), "notes body").expect("notes");
        fs::write(directory.path().join("docs/ignored.md"), "ignored body").expect("ignored");
        fs::write(
            directory.path().join("docs/generated/gen.md"),
            "generated body",
        )
        .expect("generated");
        let visibility =
            SourceVisibility::new(Vec::new(), vec!["docs/generated/**".to_owned()], true);
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect("workspace index");
        let text_paths: Vec<&str> = index
            .text_files()
            .map(|file| file.path().as_str())
            .collect();
        assert_eq!(
            text_paths,
            [".gitignore", "docs/guide.md", "docs/notes.mdx"],
            "every visible path no language claims joins the text catalog"
        );
        let source_paths: Vec<&str> = index.files().map(|file| file.path().as_str()).collect();
        assert_eq!(source_paths, ["docs/guide.md", "docs/notes.mdx"]);
    }

    /// A workspace that gives a shipped language a nonstandard pattern gets syntax
    /// facts at that extension, and loses them at the extension the provider
    /// declares - a present `include` replaces the shipped patterns.
    #[test]
    fn test_configured_include_routes_a_nonstandard_extension_to_a_shipped_provider() {
        use rift_protocol::configuration::{LanguageConfiguration, WorkspaceConfiguration};
        use rift_protocol::read::PathPattern;

        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(
            directory.path().join("component.rsx"),
            "pub fn beacon() {}\n",
        )
        .expect("nonstandard extension");
        fs::write(directory.path().join("plain.rs"), "pub fn plain() {}\n").expect("rust file");

        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "rust".to_owned(),
            LanguageConfiguration {
                include: Some(vec![PathPattern("**/*.rsx".to_owned())]),
                ..LanguageConfiguration::default()
            },
        );
        let index = WorkspaceIndex::build_with_languages(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::from(&configuration),
            &LanguageFileSelections::from(&configuration),
        )
        .expect("workspace index");

        let source_paths: Vec<&str> = index.files().map(|file| file.path().as_str()).collect();
        assert_eq!(
            source_paths,
            ["component.rsx"],
            "the configured pattern replaces the shipped one"
        );
        let component = index
            .file(&ProjectPath::new("component.rsx").expect("project path"))
            .expect("the nonstandard extension is indexed as source");
        assert!(
            component
                .syntax()
                .symbols()
                .iter()
                .any(|symbol| symbol.name == "beacon"),
            "the shipped Rust provider parses the configured extension"
        );
        let text_paths: Vec<&str> = index
            .text_files()
            .map(|file| file.path().as_str())
            .collect();
        assert_eq!(
            text_paths,
            ["component.rsx", "plain.rs"],
            "the file the pattern dropped falls through to the text lane"
        );
    }

    /// `[source]` decides visibility before any language entry is consulted, so a
    /// language `include` cannot reach an excluded path.
    #[test]
    fn test_source_exclusion_wins_over_a_language_include() {
        use rift_protocol::configuration::{LanguageConfiguration, WorkspaceConfiguration};
        use rift_protocol::read::PathPattern;

        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("vendor")).expect("directories");
        fs::write(
            directory.path().join("vendor/copy.rsx"),
            "pub fn vendored() {}\n",
        )
        .expect("excluded candidate");
        fs::write(directory.path().join("own.rsx"), "pub fn own() {}\n").expect("own candidate");

        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "rust".to_owned(),
            LanguageConfiguration {
                include: Some(vec![PathPattern("**/*.rsx".to_owned())]),
                ..LanguageConfiguration::default()
            },
        );
        let visibility = SourceVisibility::new(Vec::new(), vec!["vendor/**".to_owned()], true);
        let index = WorkspaceIndex::build_with_languages(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::from(&configuration),
            &LanguageFileSelections::from(&configuration),
        )
        .expect("workspace index");

        let source_paths: Vec<&str> = index.files().map(|file| file.path().as_str()).collect();
        assert_eq!(
            source_paths,
            ["own.rsx"],
            "an excluded path stays out whatever a language entry claims"
        );
        let text_paths: Vec<&str> = index
            .text_files()
            .map(|file| file.path().as_str())
            .collect();
        assert_eq!(
            text_paths,
            ["own.rsx"],
            "the excluded path joins no lane at all"
        );
    }

    /// Two entries claiming one current path refuse the workspace candidate, so
    /// the conflict is met before publication and names the path and both keys.
    #[test]
    fn test_two_entries_claiming_one_path_refuse_the_workspace_candidate() {
        use rift_protocol::configuration::{LanguageConfiguration, WorkspaceConfiguration};
        use rift_protocol::read::PathPattern;

        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n").expect("source file");

        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "python".to_owned(),
            LanguageConfiguration {
                include: Some(vec![PathPattern("**/*.rs".to_owned())]),
                ..LanguageConfiguration::default()
            },
        );
        let error = WorkspaceIndex::build_with_languages(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::from(&configuration),
            &LanguageFileSelections::from(&configuration),
        )
        .expect_err("a path two entries claim must refuse the candidate");

        assert_eq!(
            error.slug(),
            errors::index::workspace_language_match_conflict::SLUG
        );
        let message = error.to_string();
        assert!(
            message.contains("rust") && message.contains("python"),
            "the refusal names both entries: {message}"
        );
        assert!(
            error.context().any(|(_, value)| value.contains("lib.rs")),
            "the refusal names the conflicting path: {error:?}"
        );
    }

    /// A watcher reports the root in whatever spelling it was handed, and on macOS a
    /// temporary directory is reached through a symlink. The configuration file has to
    /// be recognized under that spelling too, or a workspace there rebuilds its whole
    /// tree for every `rift.toml` write.
    #[cfg(unix)]
    #[test]
    fn test_the_configuration_file_is_recognized_through_a_symlinked_root() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let real = directory.path().join("workspace");
        fs::create_dir(&real).expect("workspace directory");
        fs::write(real.join("rift.toml"), "[source]\n").expect("configuration file");
        fs::write(real.join("lib.rs"), "pub fn beacon() {}\n").expect("source file");
        let linked = directory.path().join("linked");
        unix_fs::symlink(&real, &linked).expect("root symlink");

        let policy = WorkspaceSourcePolicy::build(
            &linked,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("source policy");

        assert!(
            policy.is_workspace_configuration(&linked.join("rift.toml")),
            "the watched spelling names the same configuration file"
        );
        let canonical = fs::canonicalize(&real).expect("canonical workspace");
        assert!(
            policy.is_workspace_configuration(&canonical.join("rift.toml")),
            "so does the canonical spelling"
        );
        assert!(
            !policy.is_workspace_configuration(&linked.join("lib.rs")),
            "no other file is the configuration file"
        );
        assert!(
            policy.decides_inclusion(&linked.join("rift.toml")),
            "writing it decides what the workspace includes"
        );
    }

    /// Provider facts enrich the baseline content unit under the same file path.
    #[test]
    fn test_build_indexes_a_markdown_file_as_source_and_one_content_unit() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(
            directory.path().join("README.md"),
            "# Install\n\nRun the beacon.\n",
        )
        .expect("markdown fixture");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("workspace index");

        assert_eq!(index.file_count(), 1, "the provider must publish syntax");
        assert_eq!(
            index.text_file_count(),
            1,
            "the baseline catalog must hold the same file once"
        );
        let units = index.index_documents();
        assert_eq!(
            units
                .iter()
                .filter(|unit| unit.kind() == DocumentKind::TextFile)
                .count(),
            1,
            "provider content must produce one whole-file unit: {units:#?}"
        );
        assert_eq!(
            units
                .iter()
                .filter(|unit| unit.kind() == DocumentKind::Symbol)
                .count(),
            1,
            "the heading remains a syntax fact: {units:#?}"
        );
        assert!(
            units
                .iter()
                .all(|unit| document_path(unit).as_str() == "README.md"),
            "syntax and content facts must share one file path: {units:#?}"
        );
    }

    #[test]
    fn test_build_indexes_json_and_yaml_as_content_and_syntax() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(
            directory.path().join("config.json"),
            "{\"server\": {\"port\": 8080}}\n",
        )
        .expect("json fixture");
        fs::write(directory.path().join("deploy.yml"), "retries: 3\n").expect("yaml fixture");
        fs::create_dir_all(directory.path().join(".rift")).expect("state directory");
        fs::write(
            directory.path().join(".rift/server.json"),
            "{\"port\": 1}\n",
        )
        .expect("state file");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("workspace index");
        let source_paths: Vec<&str> = index.files().map(|file| file.path().as_str()).collect();
        assert_eq!(source_paths, ["config.json", "deploy.yml"]);
        assert_eq!(index.text_file_count(), 2);
        let units = index.index_documents();
        let identities: Vec<&str> = units
            .iter()
            .map(|document| document.identity().as_str())
            .collect();
        assert!(identities.contains(&"rift://symbol/json/config.json/server"));
        assert!(identities.contains(&"rift://symbol/yaml/deploy.yml/retries"));
        assert_eq!(
            units
                .iter()
                .filter(|unit| unit.kind() == DocumentKind::TextFile)
                .count(),
            2
        );
    }

    #[test]
    fn test_build_omits_invalid_utf8_source_file_and_warns() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("src")).expect("fixture directory");
        fs::write(directory.path().join("src/invalid.rs"), [0xff]).expect("invalid UTF-8 source");
        fs::write(directory.path().join("src/valid.rs"), "pub fn kept() {}\n")
            .expect("valid source");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("one invalid source file must not fail the build");

        let invalid_path = ProjectPath::new("src/invalid.rs").expect("fixture path");
        let valid_path = ProjectPath::new("src/valid.rs").expect("fixture path");
        assert!(
            index.file(&invalid_path).is_none(),
            "the invalid source file is omitted from the index"
        );
        assert!(
            index.file(&valid_path).is_some(),
            "the valid source file remains available"
        );
        assert!(
            matches!(index.warnings(), [warning]
            if warning_matches(warning, &invalid_path, errors::index::workspace_invalid_source::SLUG)),
            "the build carries a warning naming the skipped file and its error"
        );
    }

    #[test]
    fn test_build_skips_visible_utf8_file_containing_nul_byte() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("artifact.txt"), b"note\0payload").expect("binary fixture");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("binary file must not fail catalog build");
        let path = ProjectPath::new("artifact.txt").expect("fixture path");
        assert!(index.text_file(&path).is_none());
        assert_eq!(
            index.warnings(),
            &[WorkspaceIndexWarning::BinarySource(path)]
        );
    }

    #[test]
    fn test_build_skips_provider_files_that_fail_catalog_acceptance() {
        let directory = tempfile::tempdir().expect("workspace");
        fs::write(directory.path().join("binary.rs"), b"fn hidden() {}\0").expect("binary");
        fs::write(directory.path().join("large.rs"), vec![b'x'; 33]).expect("oversized");
        fs::write(directory.path().join("valid.rs"), "pub fn kept() {}\n").expect("valid");
        let limits = WorkspaceIndexLimits::new(3, 32, 1_024, 4, 5).expect("limits");
        let index = WorkspaceIndex::build(
            directory.path(),
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("invalid provider files must not hide valid file");
        assert!(
            index
                .file(&ProjectPath::new("binary.rs").expect("path"))
                .is_none()
        );
        assert!(
            index
                .file(&ProjectPath::new("large.rs").expect("path"))
                .is_none()
        );
        assert!(
            index
                .file(&ProjectPath::new("valid.rs").expect("path"))
                .is_some()
        );
        let large_path = ProjectPath::new("large.rs").expect("path");
        assert!(
            matches!(index.warnings(), [WorkspaceIndexWarning::BinarySource(binary), warning]
            if binary.as_str() == "binary.rs"
                && warning_matches(warning, &large_path, errors::index::workspace_file_too_large::SLUG))
        );
        assert_eq!(index.left_out_file_count(), 2);
    }

    /// Parenthesis nesting past the shipped syntax depth bound of 512.
    const DEEP_NESTING: usize = 600;

    /// A Rust source whose syntax tree runs deeper than the provider accepts.
    fn deep_source() -> String {
        format!(
            "pub fn deep() -> i32 {{ {open}1{close} }}\n",
            open = "(".repeat(DEEP_NESTING),
            close = ")".repeat(DEEP_NESTING),
        )
    }

    /// The lexical store records each file's content digest beside its rows, so a file the
    /// build left out still needs one: without it, every later build would count that file
    /// as new and write it again.
    #[test]
    fn test_content_digests_name_every_file_the_build_held_or_left_out() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("src")).expect("fixture directory");
        fs::write(root.join("src/lib.rs"), "pub fn kept() {}\n").expect("source");
        fs::write(root.join("src/deep.rs"), deep_source()).expect("deep source");
        fs::write(root.join("notes.txt"), "plain notes\n").expect("text file");
        let index = indexed(
            root,
            &TextFileInclusion::new(vec!["**/*.txt".to_owned()], 1_024),
        );
        let digests = index.content_digests();
        let entries: Vec<(&str, FileDigest)> = digests
            .iter()
            .map(|(path, digest)| (path.as_str(), digest))
            .collect();
        assert_eq!(
            entries,
            [
                ("notes.txt", FileDigest::of(b"plain notes\n")),
                ("src/deep.rs", FileDigest::of(deep_source().as_bytes())),
                ("src/lib.rs", FileDigest::of(b"pub fn kept() {}\n")),
            ],
            "the text file, the left-out file, and the parsed file each carry their bytes' digest"
        );
    }

    #[test]
    fn test_build_leaves_a_file_past_a_syntax_bound_out_and_keeps_the_rest() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("src")).expect("fixture directory");
        fs::write(root.join("src/lib.rs"), "pub fn kept() {}\n").expect("source");
        fs::write(root.join("src/deep.rs"), deep_source()).expect("deep source");
        let index = indexed(root, &TextFileInclusion::default());
        let deep = ProjectPath::new("src/deep.rs").expect("path");
        assert!(index.file(&deep).is_none(), "the deep file is not indexed");
        assert!(
            index.text_file(&deep).is_none(),
            "the deep file is absent from the text catalog too"
        );
        assert!(has_symbol(&index, "kept"), "the normal file still serves");
        assert_eq!(index.file_count(), 1);
        assert_eq!(index.left_out_file_count(), 1);
        // A request-time capture reads the deep file without parsing it, so the index
        // keeps that file's digest or every read would see a tree that never settles.
        let capture = capture_digests(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("the capture must read the tree");
        assert_eq!(capture.fingerprint(), index.digests().fingerprint());
        assert_eq!(
            index.digest(&deep),
            Some(FileDigest::of(deep_source().as_bytes())),
            "the left-out file's bytes still resolve an observation"
        );
        assert!(matches!(index.warnings(), [warning]
            if warning_matches(warning, &deep, errors::syntax::too_deep::SLUG)));
    }

    #[test]
    fn test_rebuild_leaves_a_file_turned_deep_out_and_holds_it_again_once_repaired() {
        let directory = fixture();
        let root = directory.path();
        let index = indexed(root, &TextFileInclusion::default());
        let lib_path = ProjectPath::new("src/lib.rs").expect("path");
        assert!(index.file(&lib_path).is_some());

        fs::write(root.join("src/lib.rs"), deep_source()).expect("deep source");
        let changes = resolved(&index, root, &["src/lib.rs"]);
        let deep = index
            .rebuilt(&changes)
            .expect("one deep file must not fail rebuild");
        assert!(deep.file(&lib_path).is_none());
        assert!(deep.text_file(&lib_path).is_none());
        assert_eq!(deep.left_out_file_count(), 1);
        assert_eq!(
            deep.digest(&lib_path),
            Some(FileDigest::of(deep_source().as_bytes()))
        );
        assert!(matches!(deep.warnings(), [warning]
            if warning_matches(warning, &lib_path, errors::syntax::too_deep::SLUG)));

        fs::write(root.join("src/lib.rs"), "pub struct Rift;\n").expect("repaired source");
        let changes = resolved(&deep, root, &["src/lib.rs"]);
        let repaired = deep
            .rebuilt(&changes)
            .expect("the repaired file must rebuild");
        assert!(
            repaired.file(&lib_path).is_some(),
            "the repaired file is held again"
        );
        assert_eq!(repaired.left_out_file_count(), 0);
        assert_eq!(repaired.digests().fingerprint(), *repaired.fingerprint());
    }

    /// A declaration named exactly `PROVIDER_SYMBOL_ID_BYTES_MAX` bytes: the document
    /// keeps it, and the identity minted from it passes the bound, so the Contribution
    /// contract refuses `provider_symbol`.
    fn wide_source() -> String {
        format!(
            "pub struct {};\n",
            "S".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX)
        )
    }

    #[test]
    fn test_build_leaves_a_file_whose_declaration_the_contract_refuses_out_and_keeps_the_rest() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("src")).expect("fixture directory");
        fs::write(root.join("src/lib.rs"), "pub fn kept() {}\n").expect("source");
        fs::write(root.join("src/wide.rs"), wide_source()).expect("wide source");
        let index = indexed(root, &TextFileInclusion::default());
        let wide = ProjectPath::new("src/wide.rs").expect("path");
        assert!(index.file(&wide).is_none(), "the wide file is not indexed");
        assert!(
            index.text_file(&wide).is_none(),
            "the wide file is absent from the text catalog too"
        );
        assert!(has_symbol(&index, "kept"), "the normal file still serves");
        assert_eq!(index.file_count(), 1);
        assert_eq!(index.left_out_file_count(), 1);
        assert!(
            matches!(index.warnings(), [WorkspaceIndexWarning::Contribution { path, error }]
            if path == &wide && error.context().any(|(key, value)| key == "field" && value == "provider_symbol"))
        );
        let capture = capture_digests(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("the capture must read the tree");
        assert_eq!(capture.fingerprint(), index.digests().fingerprint());
        assert_eq!(
            index.digest(&wide),
            Some(FileDigest::of(wide_source().as_bytes())),
            "the left-out file's bytes still resolve an observation"
        );
    }

    #[test]
    fn test_rebuild_leaves_a_file_turned_wide_out_and_holds_it_again_once_repaired() {
        let directory = fixture();
        let root = directory.path();
        let index = indexed(root, &TextFileInclusion::default());
        let lib_path = ProjectPath::new("src/lib.rs").expect("path");

        fs::write(root.join("src/lib.rs"), wide_source()).expect("wide source");
        let changes = resolved(&index, root, &["src/lib.rs"]);
        let wide = index
            .rebuilt(&changes)
            .expect("one refused declaration must not fail rebuild");
        assert!(wide.file(&lib_path).is_none());
        assert!(wide.text_file(&lib_path).is_none());
        assert_eq!(wide.left_out_file_count(), 1);
        assert!(
            matches!(wide.warnings(), [WorkspaceIndexWarning::Contribution { path, error }]
            if path == &lib_path && error.context().any(|(key, value)| key == "field" && value == "provider_symbol"))
        );
        assert_eq!(wide.digests().fingerprint(), *wide.fingerprint());

        fs::write(root.join("src/lib.rs"), "pub struct Rift;\n").expect("repaired source");
        let changes = resolved(&wide, root, &["src/lib.rs"]);
        let repaired = wide
            .rebuilt(&changes)
            .expect("the repaired file must rebuild");
        assert!(repaired.file(&lib_path).is_some());
        assert_eq!(repaired.left_out_file_count(), 0);
    }

    /// A provider error leaves a file out only when one document's Contribution was
    /// refused; an error raised elsewhere fails the build.
    #[test]
    fn test_left_out_file_names_a_refused_contribution_and_no_other_provider_fault() {
        let path = ProjectPath::new("src/wide.rs").expect("path");
        let error = errors::index::workspace_provider()
            .path(Path::new("/workspace/src/wide.rs"))
            .cause(rift_core::SourceRange::new(1, 0).expect_err("a reversed range is refused"))
            .error();
        assert_eq!(
            error_path(&error),
            Some(Path::new("/workspace/src/wide.rs").to_path_buf())
        );
        let Some(WorkspaceIndexWarning::Contribution {
            path: warning_path,
            error: warning_error,
        }) = left_out_file(error, path.clone()).expect("contribution is a left-out warning")
        else {
            panic!("contribution refusal leaves file out")
        };
        assert_eq!(warning_path, path);
        assert!(
            source_rift_error(&warning_error)
                .is_some_and(|source| source.context().any(|(key, _)| key == "field"))
        );

        let elsewhere = rift_core::SourceRange::new(1, 0)
            .expect_err("a reversed range is refused")
            .with(ctx::operation("index.normalize"));
        assert!(left_out_file(elsewhere, path).is_err());
    }

    #[test]
    fn catalog_and_syntax_share_source_until_last_reader_releases_it() {
        let path = ProjectPath::new("src/shared.rs").expect("path");
        let text = TextSourceFile::from_content(path, "pub fn original() {}\n".to_owned());
        let owner = Arc::downgrade(&text.content);
        let file = indexed_file_from_catalog(
            &text,
            Path::new("src/shared.rs"),
            &RustSyntaxProvider::default(),
            SyntaxLimits::default(),
        )
        .expect("source parses");
        assert_eq!(text.content().as_ptr(), file.source().as_ptr());
        let retained = file.clone();
        assert_eq!(retained.source().as_ptr(), file.source().as_ptr());
        drop(text);
        drop(file);
        assert_eq!(retained.source(), "pub fn original() {}\n");
        assert_eq!(retained.syntax().symbols()[0].name, "original");
        assert!(owner.upgrade().is_some(), "the last reader owns the source");
        drop(retained);
        assert!(
            owner.upgrade().is_none(),
            "no cache keeps released source alive"
        );
    }

    #[test]
    fn test_left_out_file_names_the_per_file_faults_and_no_other() {
        let path = ProjectPath::new("src/deep.rs").expect("path");
        let context = Path::new("src/deep.rs");
        let too_large = errors::index::workspace_file_too_large()
            .path(context)
            .field("source.file_bytes")
            .observed(64_usize)
            .maximum(32_usize)
            .error();
        assert!(matches!(
            left_out_file(too_large, path.clone()),
            Ok(Some(WorkspaceIndexWarning::FileTooLarge { .. }))
        ));
        let invalid_bytes = Vec::from([0xff_u8]);
        let invalid_source =
            std::str::from_utf8(&invalid_bytes).expect_err("invalid utf-8 fixture");
        let invalid = errors::index::workspace_invalid_source()
            .path(context)
            .source(invalid_source)
            .error();
        let warning = left_out_file(invalid, path.clone())
            .expect("invalid UTF-8 is a file omission")
            .expect("invalid UTF-8 names a warning");
        assert!(matches!(
            &warning,
            WorkspaceIndexWarning::InvalidUtf8Source { .. }
        ));
        assert_eq!(warning.reason(), "holds bytes that are not valid UTF-8");
        let other = errors::index::workspace_workspace_too_large()
            .field("source.workspace_bytes")
            .observed(64_usize)
            .maximum(32_usize)
            .error();
        assert!(left_out_file(other, path.clone()).is_err());
        let source = errors::index::workspace_syntax()
            .path(context)
            .source(std::io::Error::other("provider refused"))
            .error();
        assert!(
            left_out_file(source, path.clone()).is_err(),
            "a standard source without a syntax bound stays failure"
        );
        let syntax = errors::index::workspace_syntax()
            .path(context)
            .cause(
                errors::syntax::source_too_large()
                    .source_bytes(2_u64)
                    .source_bytes_max(1_u64)
                    .error(),
            )
            .error();
        assert!(
            matches!(
                left_out_file(syntax, path.clone()),
                Ok(Some(WorkspaceIndexWarning::SyntaxTooLarge { .. }))
            ),
            "syntax bound cause becomes a warning"
        );

        let strict = SyntaxLimits::new(1, 1, 1).expect("positive bounds");
        let text = TextSourceFile::from_content(path.clone(), "pub fn deep() {}\n".to_owned());
        let refused =
            indexed_file_from_catalog(&text, context, &RustSyntaxProvider::default(), strict)
                .expect_err("the syntax byte bound refuses the file");
        let Some(WorkspaceIndexWarning::SyntaxTooLarge {
            path: warning_path,
            error,
        }) = left_out_file(refused, path.clone()).expect("syntax bound is a warning")
        else {
            panic!("syntax bound leaves file out")
        };
        assert_eq!(warning_path, path);
        assert_eq!(
            source_rift_error(&error).map(RiftError::slug),
            Some(errors::syntax::source_too_large::SLUG)
        );
    }

    /// A provider failing outside its bounds: the fault is not one that leaves the file
    /// out, so the read keeps failing the build.
    #[derive(Debug)]
    struct RefusingProvider {
        language: rift_protocol::read::Language,
    }

    impl SyntaxProvider for RefusingProvider {
        fn language(&self) -> &rift_protocol::read::Language {
            &self.language
        }

        fn analyze(
            &self,
            _source: SyntaxSource<'_>,
            _limits: SyntaxLimits,
        ) -> Result<SyntaxDocument, RiftError> {
            errors::index::workspace_provider().fail()
        }

        fn node_facets(&self, _kind: &str) -> Vec<rift_protocol::read::NodeFacet> {
            Vec::new()
        }
    }

    #[test]
    fn test_syntax_read_keeps_failing_the_build_on_a_provider_fault_outside_its_bounds() {
        let path = ProjectPath::new("lib.rs").expect("valid path");
        let text = TextSourceFile::from_content(path.clone(), "pub fn beacon() {}\n".to_owned());
        let provider = RefusingProvider {
            language: rift_protocol::read::Language::from_identity_segment("rust")
                .expect("rust is a language"),
        };

        let Err(error) = syntax_read(
            &text,
            Path::new("/workspace/lib.rs"),
            &provider,
            SyntaxLimits::default(),
        ) else {
            panic!("a provider fault outside its bounds fails the build");
        };

        assert_eq!(error.slug(), errors::index::workspace_syntax::SLUG);
        assert!(left_out_file(error, path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_permission_only_change_updates_captured_and_indexed_file_state() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("workspace");
        let path = directory.path().join("script.txt");
        let project_path = ProjectPath::new("script.txt").expect("path");
        fs::write(&path, "run\n").expect("script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissions");
        let index = indexed(directory.path(), &TextFileInclusion::default());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("permissions");
        let observed = capture_digests(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("capture");
        assert_ne!(
            index.digests().get(&project_path),
            observed.get(&project_path),
            "executable metadata must change captured file state"
        );

        let changes = PathChanges::resolve(
            observed
                .iter()
                .map(|(path, digest)| (path.clone(), Some(FileRecord::Digest(digest)))),
            |path| index.digests().get(path).map(FileRecord::Digest),
        );
        let rebuilt = index.rebuilt(&changes).expect("metadata rebuild");
        assert!(
            rebuilt
                .text_file(&project_path)
                .expect("rebuilt script")
                .executable(),
            "permission-only change must refresh indexed metadata"
        );
    }

    /// The effective language table compiles before any file is read, so an
    /// invalid `[search.text].include` pattern refuses the whole scan.
    #[test]
    fn test_build_refuses_an_invalid_text_include_pattern() {
        let directory = fixture();
        let error = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::new(vec!["[".to_owned()], 1_024),
        )
        .expect_err("an unclosed character class must refuse");
        assert_eq!(error.slug(), errors::analysis::source_pattern_invalid::SLUG);
    }

    /// An empty `[search.text].include` selects no plain text, so a path no
    /// language claims joins neither lane - on the whole scan and on the
    /// incremental rebuild that follows it.
    #[test]
    fn test_an_empty_text_selection_leaves_an_unclaimed_path_out_of_both_lanes() {
        let directory = fixture();
        fs::write(directory.path().join("notes.ini"), "[section]\nkey = 1\n")
            .expect("an extension no language claims");
        let source = ProjectPath::new("src/lib.rs").expect("project path");
        let notes = ProjectPath::new("notes.ini").expect("project path");
        let prose = ProjectPath::new("README.txt").expect("project path");
        let index = indexed(directory.path(), &TextFileInclusion::new(Vec::new(), 1_024));
        assert!(
            index.file(&source).is_some(),
            "a shipped language still claims its own path"
        );
        assert!(
            index.text_file(&notes).is_none(),
            "no text pattern selects the unclaimed path"
        );
        assert!(
            index.text_file(&prose).is_none(),
            "no text pattern selects prose either"
        );

        fs::write(directory.path().join("notes.ini"), "[section]\nkey = 2\n")
            .expect("unclaimed rewrite");
        let changes = resolved(&index, directory.path(), &["notes.ini"]);
        assert_eq!(changes.len(), 1, "the rewrite is one named change");
        let rebuilt = index.rebuilt(&changes).expect("incremental rebuild");
        assert!(
            rebuilt.text_file(&notes).is_none(),
            "the rebuild drops the unclaimed path the same way the scan did"
        );
        assert!(
            rebuilt.file(&source).is_some(),
            "the rebuild keeps every claimed path it shares with the previous index"
        );
    }

    #[test]
    fn test_documentation_metadata_tracks_edit_rename_delete_and_relink() {
        use rift_protocol::documentation::{
            DocumentationLinkResolution, DocumentationSourceIdentity,
        };

        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::write(root.join("README.md"), "# Guide\n\n[Notes](notes.md)\n").expect("guide");
        fs::write(root.join("notes.md"), "# Notes\n\nOld text.\n").expect("notes");
        let inclusion = TextFileInclusion::new(Vec::new(), 1_024);
        let index = indexed(root, &inclusion);
        let path_of = |path: &str| DocumentationSourceIdentity::Project {
            path: rift_protocol::read::ProjectPath(path.to_owned()),
        };
        let link_resolves = |index: &WorkspaceIndex| {
            index.documentation().index().links.iter().any(|link| {
                link.authored == "notes.md"
                    && matches!(
                        link.resolution,
                        DocumentationLinkResolution::Resolved { .. }
                    )
            })
        };
        assert!(link_resolves(&index), "selected local source resolves");

        fs::write(root.join("notes.md"), "# Revised\n\nChanged text.\n").expect("edit source");
        fs::rename(root.join("notes.md"), root.join("moved.md")).expect("rename source");
        let changes = resolved(&index, root, &["notes.md", "moved.md"]);
        let renamed = index.rebuilt(&changes).expect("metadata rebuild");
        assert!(
            renamed
                .documentation()
                .index()
                .sources
                .iter()
                .any(|source| source.identity.source == path_of("moved.md"))
        );
        assert!(
            !renamed
                .documentation()
                .index()
                .sources
                .iter()
                .any(|source| source.identity.source == path_of("notes.md"))
        );
        assert!(
            !link_resolves(&renamed),
            "old destination becomes unresolved"
        );

        fs::write(root.join("README.md"), "# Guide\n\n[Notes](moved.md)\n").expect("relink");
        let changes = resolved(&renamed, root, &["README.md"]);
        let relinked = renamed.rebuilt(&changes).expect("relink rebuild");
        assert!(
            relinked.documentation().index().links.iter().any(|link| {
                link.authored == "moved.md"
                    && matches!(
                        link.resolution,
                        DocumentationLinkResolution::Resolved { .. }
                    )
            }),
            "new destination resolves"
        );

        fs::remove_file(root.join("moved.md")).expect("delete source");
        let changes = resolved(&relinked, root, &["moved.md"]);
        let deleted = relinked.rebuilt(&changes).expect("delete rebuild");
        assert!(
            !deleted
                .documentation()
                .index()
                .sources
                .iter()
                .any(|source| source.identity.source == path_of("moved.md"))
        );
        assert!(
            deleted
                .documentation()
                .index()
                .links
                .iter()
                .any(|link| link.authored == "moved.md"
                    && matches!(
                        link.resolution,
                        DocumentationLinkResolution::Unresolved { .. }
                    )),
            "deleted destination leaves unresolved link metadata"
        );
    }

    #[test]
    fn test_documentation_selection_rebuild_resolves_new_plain_text_target() {
        use rift_protocol::documentation::{
            DocumentationContentIdentity, DocumentationLinkResolution, DocumentationSourceIdentity,
        };
        use rift_protocol::read::SourceUnitId;

        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::write(root.join("README.md"), "# Guide\n\n[Notes](notes.txt)\n").expect("guide");
        fs::write(root.join("notes.txt"), "Plain notes.\n").expect("notes");
        let empty = TextFileInclusion::new(Vec::new(), 1_024);
        let before = indexed(root, &empty);
        assert!(
            !before
                .documentation()
                .index()
                .sources
                .iter()
                .any(|source| matches!(source.identity.source, DocumentationSourceIdentity::Project { ref path } if path.0 == "notes.txt"))
        );
        assert!(before.documentation().index().links.iter().any(|link| {
            link.authored == "notes.txt"
                && matches!(
                    link.resolution,
                    DocumentationLinkResolution::Unresolved { .. }
                )
        }));

        let all_text = TextFileInclusion::new(vec!["**".to_owned()], 1_024);
        let after = indexed(root, &all_text);
        assert!(
            after
                .documentation()
                .index()
                .sources
                .iter()
                .any(|source| matches!(source.identity.source, DocumentationSourceIdentity::Project { ref path } if path.0 == "notes.txt"))
        );
        assert!(after.documentation().index().links.iter().any(|link| {
            link.authored == "notes.txt"
                && matches!(
                    link.resolution,
                    DocumentationLinkResolution::Resolved { .. }
                )
        }));
        let package_owner = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: SourceUnitId("rift://source/cargo/example@1.0.0/src/lib.rs".into()),
            },
            cell: None,
        };
        assert_eq!(after.documentation_content(&package_owner), None);
    }

    #[test]
    fn test_build_indexes_empty_file_and_lone_byte_order_mark() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("src")).expect("fixture directory");
        fs::write(directory.path().join("src/empty.rs"), []).expect("empty source");
        fs::write(directory.path().join("src/bom.rs"), [0xef, 0xbb, 0xbf])
            .expect("byte-order-mark-only source");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("an empty file and a lone byte-order mark are both valid UTF-8");
        assert!(index.warnings().is_empty());
        assert!(
            index
                .file(&ProjectPath::new("src/empty.rs").expect("fixture path"))
                .is_some(),
            "an empty file still indexes"
        );
        assert!(
            index
                .file(&ProjectPath::new("src/bom.rs").expect("fixture path"))
                .is_some(),
            "a lone byte-order mark still indexes"
        );
    }

    #[test]
    fn test_capture_and_index_fingerprint_agree_when_a_file_is_invalid_utf8() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("src")).expect("fixture directory");
        fs::write(root.join("src/lib.rs"), "pub fn kept() {}\n").expect("valid source");
        fs::write(root.join("src/invalid.rs"), [0xff]).expect("invalid UTF-8 source");

        let index = indexed(root, &TextFileInclusion::default());
        let captured = WorkspaceFingerprint::capture(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect("a request-time capture must not fail over the same invalid file");
        assert_eq!(
            index.fingerprint(),
            &captured,
            "the index and a request-time capture omit the same invalid file and still agree"
        );
    }

    #[test]
    fn test_build_omits_invalid_utf8_text_file_and_warns() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("invalid.txt"), [0xff, 0xfe]).expect("invalid text bytes");
        fs::write(directory.path().join("valid.txt"), "kept").expect("valid text bytes");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("one invalid text file must not fail the build");

        let invalid_path = ProjectPath::new("invalid.txt").expect("fixture path");
        let valid_path = ProjectPath::new("valid.txt").expect("fixture path");
        assert!(
            index.text_file(&invalid_path).is_none(),
            "the invalid text file is omitted from the index"
        );
        assert!(
            index.text_file(&valid_path).is_some(),
            "the valid text file remains available"
        );
        assert!(
            matches!(index.warnings(), [warning]
            if warning_matches(warning, &invalid_path, errors::index::workspace_invalid_source::SLUG)),
            "the build carries a warning naming the skipped file and error"
        );
    }

    #[test]
    fn test_build_text_files_count_toward_the_shared_file_count_bound() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("a.rs"), "pub fn a() {}\n").expect("source");
        fs::write(directory.path().join("b.txt"), "text body").expect("text");
        let limits = WorkspaceIndexLimits::new(1, 1_000, 2_000, 4, 5).expect("limits");
        let error = WorkspaceIndex::build(
            directory.path(),
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect_err("one source file plus one text file must breach a one-file bound");
        assert_eq!(error.slug(), errors::index::workspace_too_many_files::SLUG);
    }

    #[test]
    fn test_text_files_accessor_returns_included_text_files() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("readme.txt"), "hello").expect("text file");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("workspace index");
        assert_eq!(index.text_file_count(), 1);
        let text = index
            .text_file(&ProjectPath::new("readme.txt").expect("fixture path must be valid"))
            .expect("the baseline text file must be indexed");
        assert_eq!(text.path().as_str(), "readme.txt");
        assert_eq!(text.content(), "hello");
    }

    #[test]
    fn test_fingerprint_changes_when_a_text_files_bytes_change_or_it_appears_or_disappears() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let empty = WorkspaceFingerprint::capture(directory.path(), limits, &visibility)
            .expect("fingerprint of an empty workspace");

        fs::write(directory.path().join("readme.txt"), "first").expect("text file");
        let appeared = WorkspaceFingerprint::capture(directory.path(), limits, &visibility)
            .expect("fingerprint after the text file appears");
        assert_ne!(
            empty, appeared,
            "a newly appeared text file must change the fingerprint"
        );

        fs::write(directory.path().join("readme.txt"), "second").expect("edited text file");
        let edited = WorkspaceFingerprint::capture(directory.path(), limits, &visibility)
            .expect("fingerprint after the text file's bytes change");
        assert_ne!(
            appeared, edited,
            "an edited text file's bytes must change the fingerprint"
        );

        fs::remove_file(directory.path().join("readme.txt")).expect("remove text file");
        let removed = WorkspaceFingerprint::capture(directory.path(), limits, &visibility)
            .expect("fingerprint after the text file disappears");
        assert_eq!(
            empty, removed,
            "removing the text file must restore the original fingerprint"
        );
    }

    #[test]
    fn test_lexical_units_symbol_identity_name_and_content_match_the_parsed_declaration() {
        let directory = fixture();
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("workspace index");
        let units = index.index_documents();
        let update = units
            .iter()
            .find(|unit| {
                unit.kind() == DocumentKind::Symbol
                    && unit.fields().get(SearchableField::Name) == Some("update")
            })
            .expect("the update symbol must produce a lexical unit");
        assert_eq!(
            update.identity().as_str(),
            "rift://symbol/rust/src/lib.rs/Rift::update"
        );
        assert_eq!(document_path(update).as_str(), "src/lib.rs");
        assert_eq!(
            update.content(),
            "",
            "a declaration's source stays in its file's document"
        );
        assert_eq!(update.byte_offset(), None);
    }

    #[test]
    fn test_lexical_units_text_file_stem_and_content_match_the_whole_file() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("guide.txt"), "guide body").expect("text file");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("workspace index");
        let units = index.index_documents();
        let text_unit = units
            .iter()
            .find(|unit| unit.kind() == DocumentKind::TextFile)
            .expect("the text file must produce a lexical unit");
        assert_eq!(text_unit.identity().as_str(), "guide.txt");
        assert_eq!(
            text_unit.fields().get(SearchableField::Name),
            Some("guide.txt")
        );
        assert_eq!(
            text_unit.fields().get(SearchableField::FileContent),
            Some("guide body")
        );
        assert!(
            index.chunked_text_files().is_empty(),
            "a file within the chunk bound must not be reported as chunked"
        );
    }

    #[test]
    fn test_lexical_units_chunks_an_oversized_text_file_and_chunked_text_files_reports_it() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        // Five lines of four bytes each: a 10-byte chunk bound packs two lines per chunk,
        // so the eight-line file below must split into several chunk units.
        let content = "aaa\nbbb\nccc\nddd\neee\nfff\nggg\nhhh\n";
        fs::write(directory.path().join("big.txt"), content).expect("oversized text file");
        let inclusion = TextFileInclusion::new(vec!["**".to_owned()], 10);
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &inclusion,
        )
        .expect("workspace index");

        let units: Vec<_> = index
            .index_documents()
            .into_iter()
            .filter(|unit| unit.kind() == DocumentKind::TextFile)
            .collect();
        assert_eq!(
            units.len(),
            4,
            "an 8-line file chunked two lines at a time yields 4 units"
        );
        let identities: Vec<&str> = units
            .iter()
            .map(|document| document.identity().as_str())
            .collect();
        assert_eq!(
            identities,
            ["big.txt#0", "big.txt#1", "big.txt#2", "big.txt#3"]
        );
        let rejoined: String = units
            .iter()
            .map(|document| {
                document
                    .fields()
                    .get(SearchableField::FileContent)
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            rejoined, content,
            "chunk content must reconstruct the file exactly"
        );
        for unit in &units {
            assert_eq!(unit.fields().get(SearchableField::Name), Some("big.txt"));
            assert_eq!(document_path(unit).as_str(), "big.txt");
        }

        let chunked = index.chunked_text_files();
        assert_eq!(chunked.len(), 1);
        assert_eq!(chunked[0].0.as_str(), "big.txt");
        assert_eq!(chunked[0].1, 4);
    }

    #[test]
    fn test_read_text_file_of_directory_path_reports_filesystem_failure() {
        let directory = fixture();
        let mut workspace_bytes = 0_usize;
        let error = read_text_file(
            directory.path(),
            &directory.path().join("src"),
            WorkspaceIndexLimits::default(),
            &mut workspace_bytes,
        )
        .expect_err("reading a directory's bytes as a file must fail");
        assert_eq!(error.slug(), errors::index::workspace_filesystem::SLUG);
    }

    #[test]
    fn test_read_text_file_of_path_outside_root_reports_invalid_path() {
        let directory = fixture();
        let outside = tempfile::tempdir().expect("outside workspace");
        let outside_file = outside.path().join("outside.md");
        fs::write(&outside_file, "outside text").expect("outside fixture file");
        let mut workspace_bytes = 0_usize;
        let error = read_text_file(
            directory.path(),
            &outside_file,
            WorkspaceIndexLimits::default(),
            &mut workspace_bytes,
        )
        .expect_err("a path outside root must fail to strip its prefix");
        assert_eq!(error.slug(), errors::index::workspace_invalid_path::SLUG);
    }

    #[test]
    fn test_included_text_file_over_limit_without_overflow_reports_workspace_too_large() {
        let limits = WorkspaceIndexLimits::new(5, 1_000, 10, 4, 5).expect("limits");
        let mut workspace_bytes = 6_usize;
        let project_path = ProjectPath::new("big.md").expect("fixture path");
        let error = included_text_file(
            project_path,
            b"12345".to_vec(),
            Path::new("big.md"),
            limits,
            &mut workspace_bytes,
        )
        .expect_err(
            "6 already-counted bytes plus 5 more must cross a ten-byte bound without overflowing",
        );
        assert_eq!(
            error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
    }

    #[test]
    fn test_included_file_over_limit_without_overflow_reports_workspace_too_large() {
        let limits = WorkspaceIndexLimits::new(5, 1_000, 10, 4, 5).expect("limits");
        let mut workspace_bytes = 6_usize;
        let project_path = ProjectPath::new("big.rs").expect("fixture path");
        let error = included_file(
            project_path,
            b"12345".to_vec(),
            Path::new("big.rs"),
            &RustSyntaxProvider::default(),
            limits,
            &mut workspace_bytes,
        )
        .expect_err(
            "6 already-counted bytes plus 5 more must cross a ten-byte bound without overflowing",
        );
        assert_eq!(
            error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
        assert_eq!(workspace_bytes, 11, "the refused file's bytes stay counted");
        let message = error.to_string();
        assert!(
            message.contains(SOURCE_WORKSPACE_SIZE_FIELD) && message.contains("big.rs"),
            "the refusal names the bound and the file: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_visible_digests_refuse_a_file_the_process_cannot_read() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("workspace");
        fs::write(directory.path().join("kept.txt"), "kept\n").expect("readable file");
        let sealed = directory.path().join("sealed.txt");
        fs::write(&sealed, "sealed\n").expect("sealed file");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000))
            .expect("fixture permissions set");
        let policy = WorkspaceSourcePolicy::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("source policy");
        let outcome = policy.visible_digests();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o644))
            .expect("fixture permissions restore");
        let error = outcome.expect_err("a read this process cannot make fails the capture");
        assert_eq!(error.slug(), errors::index::workspace_filesystem::SLUG);
        assert!(
            error_path(&error).is_some_and(|path| path.ends_with("sealed.txt")),
            "the refusal names the unreadable file: {error}"
        );
    }

    #[test]
    fn test_visible_digests_leave_out_a_file_past_the_per_file_bound() {
        let directory = tempfile::tempdir().expect("workspace");
        fs::write(directory.path().join("small.txt"), "kept\n").expect("small file");
        fs::write(directory.path().join("large.txt"), "x".repeat(64)).expect("large file");
        let limits = WorkspaceIndexLimits::new(8, 16, 1_024, 8, 8).expect("bounds");
        let policy = WorkspaceSourcePolicy::build(
            directory.path(),
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("source policy");
        let digests = policy
            .visible_digests()
            .expect("a file past the per-file bound is absent, never a refusal");
        let paths: Vec<&str> = digests.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["small.txt"]);
        let large = policy
            .visible_digest(&directory.path().join("large.txt"))
            .expect_err("a single-file digest read still refuses the oversized file");
        assert_eq!(large.slug(), errors::index::workspace_file_too_large::SLUG);
    }

    #[test]
    fn test_default_limits_hold_large_files_as_the_served_default_does() {
        let limits = WorkspaceIndexLimits::default();
        let served = TextFileInclusion::default().large_files();
        assert_eq!(served, LargeFileStrategy::Split);
        assert_eq!(limits, limits.with_large_files(served));
        assert_eq!(
            limits.file_bytes_max(),
            limits.workspace_bytes_max(),
            "split holds a file as text up to the workspace byte bound"
        );
        let syntax = SyntaxLimits::new(4_096, 250_000, 512).expect("syntax bounds");
        assert_eq!(
            limits.with_syntax(syntax).file_bytes_max(),
            limits.workspace_bytes_max(),
            "a new source bound keeps the strategy the bounds carry"
        );
        let skipped = limits
            .with_large_files(LargeFileStrategy::Skip)
            .with_syntax(syntax);
        assert_eq!(
            skipped.file_bytes_max(),
            4_096,
            "skip follows the source bound"
        );
    }

    #[test]
    fn test_with_workspace_bounds_replaces_the_three_source_bounds_alone() {
        let base = WorkspaceIndexLimits::new(8, 16, 1_024, 4, 32).expect("bounds");
        assert_eq!(
            base.declarations_max(),
            WORKSPACE_DECLARATIONS_MAX_DEFAULT,
            "a build states its declaration bound through the `[source]` table alone"
        );
        let bounded = base
            .with_workspace_bounds(2_000, 4_096, 64)
            .expect("positive bounds");
        assert_eq!(bounded.files_max(), 2_000);
        assert_eq!(bounded.workspace_bytes_max(), 4_096);
        assert_eq!(bounded.declarations_max(), 64);
        assert_eq!(bounded.file_bytes_max(), 16);
        assert_eq!(bounded.directory_depth_max(), 4);
        assert_eq!(bounded.results_max(), 32);
        for zero in [
            base.with_workspace_bounds(0, 4_096, 64),
            base.with_workspace_bounds(2_000, 4_096, 0),
        ] {
            let error = zero.expect_err("a zero bound refuses");
            assert_eq!(error.slug(), errors::index::workspace_zero_limit::SLUG);
        }
    }

    #[test]
    fn test_source_bound_refusals_name_the_configuration_key() {
        let directory = tempfile::tempdir().expect("workspace");
        fs::write(directory.path().join("one.rs"), "pub fn one() {}\n").expect("source");
        fs::write(directory.path().join("two.rs"), "pub fn two() {}\n").expect("source");
        let one_file = WorkspaceIndexLimits::new(1, 1_000, 2_000, 4, 5).expect("limits");
        let files = WorkspaceIndex::build(
            directory.path(),
            one_file,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect_err("two files refuse a one-file bound");
        assert_eq!(files.slug(), errors::index::workspace_too_many_files::SLUG);
        let rendered = files.to_string();
        assert!(
            rendered.contains("accepted limit of 1") && rendered.contains("field source.files"),
            "{rendered}"
        );
        assert_eq!(
            limit_evidence(&files).map(|(field, _, _)| field),
            Some("source.files".to_owned())
        );

        let ten_bytes = WorkspaceIndexLimits::new(5, 1_000, 10, 4, 5).expect("limits");
        let bytes = WorkspaceIndex::build(
            directory.path(),
            ten_bytes,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect_err("sixteen bytes refuse a ten-byte bound");
        assert_eq!(
            bytes.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
        let rendered = bytes.to_string();
        assert!(
            rendered.contains("accepted limit of 10")
                && rendered.contains("field source.workspace_size"),
            "{rendered}"
        );
    }

    #[test]
    fn test_included_text_file_refuses_invalid_utf8_rather_than_empty_content() {
        let limits = WorkspaceIndexLimits::default();
        let mut workspace_bytes = 0_usize;
        let project_path = ProjectPath::new("invalid.txt").expect("fixture path");
        let error = included_text_file(
            project_path,
            vec![0xff, 0xfe],
            Path::new("invalid.txt"),
            limits,
            &mut workspace_bytes,
        )
        .expect_err("invalid UTF-8 bytes refuse rather than indexing empty content");
        assert_eq!(error.slug(), errors::index::workspace_invalid_source::SLUG);
    }

    #[test]
    fn test_a_document_the_shape_refuses_is_left_out_rather_than_failing_the_build() {
        // `ProjectPath::new("")` is valid and names the workspace root, so a whole-file
        // document for it builds its identity from that empty path, which the document
        // shape refuses. A refusal leaves the document out; it does not stop the build,
        // because one such path would otherwise cost the workspace its whole index.
        let file = TextSourceFile {
            path: ProjectPath::new("").expect("an empty project path names the workspace root"),
            digest: FileDigest::of(b"hello"),
            content: Arc::new("hello".to_owned()),
            executable: false,
        };
        let mut documents = Vec::new();
        push_text_documents(&mut documents, &file, 1_024, &mut LeftOut::default());
        assert!(
            documents.is_empty(),
            "a refused document is left out, not published"
        );
    }

    /// One source file declaring `count` functions, as the catalog holds it.
    fn declaring_file(name: &str, count: usize) -> TextSourceFile {
        let mut content = String::new();
        for index in 0..count {
            writeln!(content, "pub fn beacon_{index}() {{}}").expect("a string write succeeds");
        }
        TextSourceFile {
            path: ProjectPath::new(name.to_owned()).expect("a project path"),
            digest: FileDigest::of(content.as_bytes()),
            content: Arc::new(content),
            executable: false,
        }
    }

    /// A workspace declaring more than the publication holds keeps the files that fit and
    /// leaves the rest out, so a large workspace is served rather than refused. The
    /// production bound is a million declarations, which no test tree reaches, so the build
    /// takes the bound as a parameter and this case passes one it can cross.
    #[test]
    fn test_a_workspace_past_the_declaration_bound_publishes_what_fits() {
        let root = tempfile::tempdir().expect("temporary directory");
        let provider = registry::provider_for_extension("rs").expect("the rust provider");
        let mut contents = IndexContents::default();
        for name in ["a.rs", "b.rs", "c.rs"] {
            let file = declaring_file(name, 3);
            contents
                .hold_source_file(
                    file,
                    &root.path().join(name),
                    provider,
                    SyntaxLimits::default(),
                )
                .expect("the catalog holds the file");
        }
        let built = built_contents(root.path(), contents.sorted(), 4, None)
            .expect("the build publishes the files that fit");
        assert_eq!(
            built
                .files
                .keys()
                .map(ProjectPath::as_str)
                .collect::<Vec<_>>(),
            ["a.rs"],
            "the first file fits the bound and the pass stops at the next one"
        );
        assert_eq!(
            built
                .left_out
                .keys()
                .map(ProjectPath::as_str)
                .collect::<Vec<_>>(),
            ["b.rs", "c.rs"],
            "every file past the bound keeps its digests as a left-out file"
        );
        let beyond = built
            .warnings
            .iter()
            .filter(|warning| matches!(warning, WorkspaceIndexWarning::DeclarationsBeyondBound(_)))
            .map(|warning| warning.path().as_str())
            .collect::<Vec<_>>();
        assert_eq!(beyond, ["b.rs", "c.rs"]);
        assert_eq!(
            built.warnings[0].reason(),
            "stands past the declarations the index holds (source.declarations)"
        );
    }

    /// A workspace inside the bound leaves nothing out, so the bound never costs a file an
    /// index that had room for it.
    #[test]
    fn test_a_workspace_within_the_declaration_bound_leaves_nothing_out() {
        let root = tempfile::tempdir().expect("temporary directory");
        let provider = registry::provider_for_extension("rs").expect("the rust provider");
        let mut contents = IndexContents::default();
        for name in ["a.rs", "b.rs"] {
            let file = declaring_file(name, 3);
            contents
                .hold_source_file(
                    file,
                    &root.path().join(name),
                    provider,
                    SyntaxLimits::default(),
                )
                .expect("the catalog holds the file");
        }
        let built = built_contents(root.path(), contents.sorted(), 6, None)
            .expect("the build publishes every file");
        assert_eq!(built.files.len(), 2);
        assert!(built.left_out.is_empty());
        assert!(built.warnings.is_empty());
    }

    #[test]
    fn refused_python_contributions_leave_out_in_one_batch() {
        let root = tempfile::tempdir().expect("temporary workspace");
        let provider = registry::provider_for_extension("py").expect("the Python provider");
        let limits = SyntaxLimits::default();
        let mut contents = IndexContents::default();
        let valid_path = ProjectPath::new("src/valid.py").expect("valid path");
        let valid_content = "def beacon():\n    return 1\n".to_owned();
        let valid = TextSourceFile {
            path: valid_path,
            digest: FileDigest::of(valid_content.as_bytes()),
            content: Arc::new(valid_content),
            executable: false,
        };
        contents
            .hold_source_file(
                valid.clone(),
                &root.path().join("src/valid.py"),
                provider,
                limits,
            )
            .expect("valid Python source");

        let name = "S".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX);
        for path in ["src/wide_a.py", "src/wide_b.py"] {
            let content = format!("def {name}():\n    return 1\n");
            let project_path = ProjectPath::new(path).expect("wide path");
            let file = TextSourceFile {
                path: project_path,
                digest: FileDigest::of(content.as_bytes()),
                content: Arc::new(content),
                executable: false,
            };
            contents
                .hold_source_file(file, &root.path().join(path), provider, limits)
                .expect("provider parses exact-bound name");
        }

        let mut expected = contents.clone();
        for path in ["src/wide_a.py", "src/wide_b.py"] {
            let path = ProjectPath::new(path).expect("path");
            assert!(
                expected.leave_out_held(
                    &path,
                    WorkspaceIndexWarning::Contribution {
                        path: path.clone(),
                        error: Arc::new(
                            errors::core::contribution_invalid_name()
                                .field("provider_symbol")
                                .error()
                        ),
                    },
                )
            );
        }
        let built = built_contents(root.path(), contents.sorted(), 10, None)
            .expect("all refused declarations leave out together");
        let expected = built_contents(root.path(), expected.sorted(), 10, None)
            .expect("valid source alone builds");
        assert_eq!(built.left_out.len(), 2);
        assert!(matches!(built.warnings.as_slice(), [
            WorkspaceIndexWarning::Contribution { path: first, error: first_error },
            WorkspaceIndexWarning::Contribution { path: second, error: second_error },
        ] if first.as_str() == "src/wide_a.py"
            && second.as_str() == "src/wide_b.py"
            && first_error.context().any(|(key, value)| key == "field" && value == "provider_symbol")
            && second_error.context().any(|(key, value)| key == "field" && value == "provider_symbol")));
        assert_eq!(
            built.semantics.graph().records(),
            expected.semantics.graph().records(),
            "accepted graph matches build without refused files"
        );
    }

    #[test]
    fn test_a_path_the_wire_can_address_publishes_a_document() {
        // The document shape's address ceiling is the wire's own, so a path at the
        // longest a project path may be still publishes. A shorter ceiling here would
        // leave a legal file out of the corpus.
        let deep = std::iter::repeat_n("d".repeat(49), 19)
            .collect::<Vec<_>>()
            .join("/");
        let path = format!("{deep}/notes.md");
        assert!(
            path.len() > 512,
            "the fixture path must be longer than a short address ceiling: {}",
            path.len()
        );
        let file = TextSourceFile {
            path: ProjectPath::new(path).expect("a long project path is legal"),
            digest: FileDigest::of(b"hello"),
            content: Arc::new("hello".to_owned()),
            executable: false,
        };
        let mut documents = Vec::new();
        push_text_documents(&mut documents, &file, 1_024, &mut LeftOut::default());
        assert_eq!(
            documents.len(),
            1,
            "a legal path publishes rather than being left out"
        );
    }

    /// A workspace whose `src/lib.rs` calls a function `src/run.rs` defines.
    fn cross_unit_fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("src")).expect("fixture directory");
        fs::write(
            directory.path().join("src/lib.rs"),
            "mod run;\nuse run::helper;\npub fn beacon() {\n    helper();\n}\n",
        )
        .expect("fixture source");
        fs::write(directory.path().join("src/run.rs"), "pub fn helper() {}\n")
            .expect("fixture source");
        directory
    }

    /// No shipped provider resolves a reference to the declaration it names, so the
    /// normalized graph carries no reference whatever the source spells.
    #[test]
    fn test_workspace_index_publishes_no_resolved_reference() {
        let directory = cross_unit_fixture();
        let index = indexed(directory.path(), &TextFileInclusion::default());
        assert!(
            index.normalized_graph().references().is_empty(),
            "the syntax publication carries declarations alone"
        );
        assert!(index.relationships().is_empty());
    }

    #[test]
    fn test_workspace_index_publishes_the_syntax_provider_alone() {
        let directory = cross_unit_fixture();
        let index = indexed(directory.path(), &TextFileInclusion::default());
        let syntax =
            rift_core::ProviderId::new(rift_syntax::SYNTAX_PROVIDER_ID).expect("provider identity");
        let publications = index.normalized_graph().publications();
        assert!(publications.provider(&syntax).is_some());
        assert_eq!(
            publications.provider_count(),
            1,
            "syntax is the one provider an index build publishes"
        );
    }

    /// Captures `root` under the default policies, reusing what `last` recorded.
    fn captured_after(root: &Path, last: &LastCapture) -> (WorkspaceDigests, LastCapture) {
        capture_digests_with_languages(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            last,
        )
        .expect("the capture must read the tree")
    }

    /// The paths the production discovery policy admits in one test workspace.
    fn discovered_files_for_test(root: &Path) -> Vec<PathBuf> {
        let languages = LanguageFileSelections::default();
        let text_inclusion = TextFileInclusion::default();
        let language = WorkspaceLanguagePolicy::build(root, &languages, &text_inclusion)
            .expect("language policy");
        let paths = discover_cancellable(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &language,
            &|| false,
        )
        .expect("discover fixture paths");
        paths
            .source
            .into_iter()
            .map(|(path, _)| path)
            .chain(paths.text)
            .chain(paths.lockfiles)
            .collect()
    }

    /// Waits until a filesystem marker has newer modification and status-change times than
    /// every path, so the next capture can prove unchanged digests safe to reuse.
    #[cfg(unix)]
    fn advance_capture_clock(root: &Path, paths: &[PathBuf]) {
        use std::os::unix::fs::MetadataExt as _;

        fs::create_dir_all(root.join(".rift")).expect("state directory");
        let marker = root.join(".rift/stat-probe");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            fs::write(&marker, b"timestamp probe").expect("advance filesystem timestamp");
            let marker_metadata = fs::metadata(&marker).expect("marker stat");
            let marker_changed = (marker_metadata.ctime(), marker_metadata.ctime_nsec());
            let marker_modified = marker_metadata.modified().expect("marker mtime");
            let old_files = paths.iter().all(|path| {
                let metadata = fs::metadata(path).expect("source stat");
                metadata.modified().expect("source mtime") < marker_modified
                    && (metadata.ctime(), metadata.ctime_nsec()) < marker_changed
            });
            if old_files {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "filesystem timestamps advance beyond every fixture file"
            );
            std::thread::yield_now();
        }
    }

    /// A two-file workspace, `src/lib.rs` and `notes.txt`, and the modification time
    /// `src/lib.rs` holds.
    fn two_file_workspace() -> (tempfile::TempDir, std::time::SystemTime) {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("src")).expect("fixture directory");
        fs::write(root.join("src/lib.rs"), "pub fn kept() {}\n").expect("source");
        fs::write(root.join("notes.txt"), "plain notes\n").expect("text");
        let modified = fs::metadata(root.join("src/lib.rs"))
            .and_then(|metadata| metadata.modified())
            .expect("modification time");
        (directory, modified)
    }

    /// Rewrites `path` in place, keeping its inode, and sets its modification time.
    fn rewrite(path: &Path, bytes: &[u8], modified: std::time::SystemTime) {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .expect("open for rewrite");
        std::io::Write::write_all(&mut file, bytes).expect("rewrite");
        file.set_modified(modified).expect("set modification time");
    }

    #[test]
    fn visible_capture_reuses_selected_reads_and_retains_refused_raw_bytes() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        let entries: [(&str, &[u8]); 4] = [
            ("lib.rs", b"pub fn kept() {}\n"),
            ("binary.rs", b"source\0bytes"),
            ("invalid.rs", &[0xff, 0xfe]),
            ("opaque.bin", b"unclassified bytes"),
        ];
        for (path, bytes) in entries {
            fs::write(root.join(path), bytes).expect("fixture bytes");
        }
        let text = TextFileInclusion::new(vec!["**/*.txt".to_owned()], 1_024);
        let limits = WorkspaceIndexLimits::default();
        let capture = |last: &LastCapture| {
            capture_visible_digests_with_languages_cancellable(
                root,
                limits,
                &SourceVisibility::default(),
                &text,
                &LanguageFileSelections::default(),
                last,
                &|| false,
            )
            .expect("bounded visible capture")
        };
        #[cfg(unix)]
        advance_capture_clock(
            root,
            &entries
                .iter()
                .map(|(path, _)| root.join(path))
                .collect::<Vec<_>>(),
        );
        let (indexed, visible, last) = capture(&LastCapture::default());
        assert_eq!(
            last.read_paths(),
            entries.len(),
            "selected files are not read twice"
        );
        assert_eq!(
            indexed.iter().count(),
            1,
            "only valid selected source is indexed"
        );
        for (path, bytes) in entries {
            assert_eq!(
                visible.get(&ProjectPath::new(path).expect("path")),
                Some(FileDigest::of(bytes)),
                "raw content includes refused and unclassified files",
            );
        }
        let (again_indexed, again_visible, next) = capture(&last);
        assert_eq!(again_indexed, indexed);
        assert_eq!(again_visible, visible);
        #[cfg(unix)]
        if crate::capture::supports_stat_reuse(root) {
            assert_eq!(
                next.read_paths(),
                0,
                "unchanged raw content uses the original stat proof"
            );
        }
        let binary = root.join("binary.rs");
        let modified = fs::metadata(&binary)
            .expect("binary stat")
            .modified()
            .expect("mtime");
        rewrite(&binary, b"changed\0raw!", modified);
        let (_, changed, reused) = capture(&next);
        assert!(
            reused.read_paths() >= 1,
            "restored modification time does not hide raw edits"
        );
        assert_ne!(changed, visible);
        let (_, cold, _) = capture(&LastCapture::default());
        assert_eq!(changed, cold, "stat reuse equals a fresh raw read");
    }

    #[test]
    fn visible_capture_enforces_raw_file_count_byte_bounds_and_cancellation() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        for path in ["a.bin", "b.bin"] {
            fs::write(root.join(path), b"raw\0").expect("raw source");
        }
        let text = TextFileInclusion::new(Vec::new(), 1_024);
        let capture = |limits, cancelled: &(dyn Fn() -> bool + Sync)| {
            capture_visible_digests_with_languages_cancellable(
                root,
                limits,
                &SourceVisibility::default(),
                &text,
                &LanguageFileSelections::default(),
                &LastCapture::default(),
                cancelled,
            )
        };
        let limits = |files, file_bytes, total_bytes| {
            WorkspaceIndexLimits::new(files, file_bytes, total_bytes, 16, 100).expect("bounds")
        };
        let error = capture(limits(1, 4, 8), &|| false)
            .expect_err("visible raw files retain the file-count bound");
        assert_eq!(error.slug(), errors::index::workspace_too_many_files::SLUG);
        let (indexed, visible, _) = capture(limits(2, 4, 7), &|| false)
            .expect("unindexed raw bytes do not count against the indexed source budget");
        assert!(indexed.is_empty());
        assert_eq!(visible.iter().count(), 2);
        let (_, visible, last) =
            capture(limits(2, 3, 8), &|| false).expect("oversized files left out");
        assert!(
            visible.is_empty(),
            "no digest describes truncated raw bytes"
        );
        assert_eq!(last.read_paths(), 2);
        let error = capture(limits(2, 4, 8), &|| true).expect_err("cancelled capture");
        assert_eq!(error.slug(), errors::index::workspace_cancelled::SLUG);
        for (path, bytes) in [("a.rs", b"// a"), ("b.rs", b"// b")] {
            fs::write(root.join(path), bytes).expect("selected source");
        }
        let error = capture(limits(4, 4, 7), &|| false)
            .expect_err("indexed source still counts against the workspace-byte bound");
        assert_eq!(
            error.slug(),
            errors::index::workspace_workspace_too_large::SLUG
        );
        let (indexed, visible, _) = capture(limits(4, 4, 8), &|| false)
            .expect("source at its bound and raw bytes beyond it remain admitted");
        assert_eq!(indexed.iter().count(), 2);
        assert_eq!(visible.iter().count(), 4);
    }

    #[test]
    fn test_capture_of_an_unchanged_tree_preserves_fingerprint() {
        let (directory, _) = two_file_workspace();
        let root = directory.path();
        let (first, last) = captured_after(root, &LastCapture::default());
        assert_eq!(
            last.read_paths(),
            2,
            "the first capture reads every admitted path: {:?}",
            discovered_files_for_test(root)
        );
        let (second, next) = captured_after(root, &last);
        assert!(
            next.read_paths() <= 2,
            "a proved boundary reuses both files; unavailable timestamp proof reads both"
        );
        assert_eq!(second.fingerprint(), first.fingerprint());
        let (_, again) = captured_after(root, &next);
        assert!(
            again.read_paths() <= 2,
            "reuse keeps its original proof; no proof reads both files"
        );
    }

    #[test]
    fn test_capture_tracks_case_distinct_paths_and_case_only_rename() {
        use same_file::Handle;

        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        let source_directory = root.join("src");
        fs::create_dir(&source_directory).expect("source directory");
        let upper = source_directory.join("Case.rs");
        let lower = source_directory.join("case.rs");
        let intermediate = source_directory.join("rename.rs");
        fs::write(&upper, "pub fn upper() {}\n").expect("upper-case source");

        let (first, first_capture) = captured_after(root, &LastCapture::default());
        assert_eq!(first.len(), 1, "first capture records one source path");
        assert_eq!(first_capture.read_paths(), 1, "first capture reads source");

        fs::write(&lower, "pub fn lower() {}\n").expect("lower-case source");
        let upper_identity = Handle::from_path(&upper).expect("open upper-case path identity");
        let lower_identity = Handle::from_path(&lower).expect("open lower-case path identity");
        let paths_are_distinct = upper_identity != lower_identity;
        let (both_spellings, both_capture) = captured_after(root, &first_capture);
        assert_eq!(
            both_spellings.len(),
            if paths_are_distinct { 2 } else { 1 },
            "capture follows filesystem path identity for case-distinct names"
        );
        if paths_are_distinct {
            assert!(
                both_spellings
                    .get(&ProjectPath::new("src/Case.rs").expect("upper project path"))
                    .is_some(),
                "case-sensitive filesystem retains upper-case path"
            );
            assert!(
                both_spellings
                    .get(&ProjectPath::new("src/case.rs").expect("lower project path"))
                    .is_some(),
                "case-sensitive filesystem retains lower-case path"
            );
        } else {
            assert!(
                both_spellings
                    .iter()
                    .all(|(path, _)| { matches!(path.as_str(), "src/Case.rs" | "src/case.rs") }),
                "case-insensitive filesystem records one spelling of shared path"
            );
            assert_eq!(
                fs::read(&upper).expect("read upper-case alias"),
                fs::read(&lower).expect("read lower-case alias"),
                "case-insensitive names resolve to same file"
            );
        }
        if paths_are_distinct {
            fs::remove_file(&lower).expect("remove lower-case path before rename");
        }
        fs::rename(&upper, &intermediate).expect("rename source to intermediate spelling");
        fs::rename(&intermediate, &lower).expect("rename source to lower-case spelling");
        fs::write(&lower, "pub fn renamed() {}\n").expect("write renamed source");

        let (renamed, renamed_capture) = captured_after(root, &both_capture);
        assert_eq!(renamed.len(), 1, "renamed tree contains one source path");
        assert!(
            renamed
                .get(&ProjectPath::new("src/Case.rs").expect("upper project path"))
                .is_none(),
            "capture drops the old path spelling"
        );
        assert!(
            renamed
                .get(&ProjectPath::new("src/case.rs").expect("lower project path"))
                .is_some(),
            "capture records the renamed path spelling"
        );
        assert_eq!(renamed_capture.read_paths(), 1, "renamed file is read");

        let cold = WorkspaceIndex::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("cold build after case-only rename");
        assert_eq!(
            renamed,
            cold.digests(),
            "incremental capture matches cold build after case-only rename"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_capture_reuses_files_after_filesystem_timestamp_advances() {
        let (directory, _) = two_file_workspace();
        let root = directory.path();
        let paths = [root.join("src/lib.rs"), root.join("notes.txt")];
        if !crate::capture::supports_stat_reuse(root) {
            let (first, last) = captured_after(root, &LastCapture::default());
            let (second, next) = captured_after(root, &last);
            assert_eq!(
                next.read_paths(),
                last.read_paths(),
                "unsupported filesystem type uses full reads on every capture"
            );
            assert_eq!(second.fingerprint(), first.fingerprint());
            return;
        }
        advance_capture_clock(root, &paths);

        let (first, last) = captured_after(root, &LastCapture::default());
        assert_eq!(
            last.read_paths(),
            2,
            "first capture reads each fixture file"
        );
        let (second, next) = captured_after(root, &last);
        assert_eq!(
            next.read_paths(),
            0,
            "strictly older files reuse captured digests"
        );
        assert_eq!(second.fingerprint(), first.fingerprint());
    }

    #[test]
    fn test_capture_does_not_recreate_a_removed_root() {
        let parent = tempfile::tempdir().expect("parent directory");
        let root = parent.path().join("removed");
        let error = capture_digests(
            &root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect_err("a removed root refuses capture");
        assert_eq!(error.slug(), errors::index::workspace_invalid_root::SLUG);
        assert!(!root.exists(), "capture must not recreate a removed root");
    }

    #[cfg(unix)]
    #[test]
    fn test_capture_does_not_reuse_across_a_symlinked_state_directory() {
        use std::os::unix::fs::symlink;

        let (directory, _) = two_file_workspace();
        let root = directory.path();
        let (_, last) = captured_after(root, &LastCapture::default());
        let state = root.join(rift_core::constants::RIFT_STATE_DIRECTORY);
        fs::create_dir_all(&state).expect("state directory");
        fs::remove_dir_all(&state).expect("remove state directory");
        let external = tempfile::tempdir().expect("external state directory");
        symlink(external.path(), &state).expect("symlink state directory");

        let (_, next) = captured_after(root, &last);
        assert_eq!(
            next.read_paths(),
            2,
            "a state path that leaves the workspace filesystem disables reuse"
        );
    }

    /// A capture record prints how many paths it holds and how many it read, never the
    /// paths or their digests.
    #[test]
    fn test_a_capture_record_prints_its_counts() {
        let (directory, _) = two_file_workspace();
        let root = directory.path();
        let (_, last) = captured_after(root, &LastCapture::default());
        assert_eq!(format!("{last:?}"), "LastCapture { paths: 2, read: 2, .. }");
        let (_, next) = captured_after(root, &last);
        assert!(
            [0, 2].contains(&next.read_paths()),
            "capture record counts reuse or full-read fallback"
        );
    }

    /// A request-time capture refuses a language selection the build refuses, before it
    /// reads a file.
    #[test]
    fn test_capture_refuses_a_language_selection_the_build_refuses() {
        let (directory, _) = two_file_workspace();
        let mut configuration = rift_protocol::configuration::WorkspaceConfiguration::default();
        configuration.languages.insert(
            "ruby".to_owned(),
            rift_protocol::configuration::LanguageConfiguration::default(),
        );
        let languages = LanguageFileSelections::from(&configuration);
        let error = capture_digests_with_languages(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &languages,
            &LastCapture::default(),
        )
        .expect_err("an unshipped language names no include");
        assert_eq!(
            error.slug(),
            errors::index::workspace_language_include_required::SLUG
        );
    }

    /// A request-time capture refuses a directory the process cannot read, naming it, as
    /// the build does.
    #[cfg(unix)]
    #[test]
    fn test_capture_refuses_a_directory_the_process_cannot_read() {
        use std::os::unix::fs::PermissionsExt as _;

        let (directory, _) = two_file_workspace();
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let locked = root.join("locked");
        fs::create_dir(&locked).expect("locked directory");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("remove read");
        let outcome = capture_digests_with_languages(
            &root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            &LastCapture::default(),
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restore read");
        let error = outcome.expect_err("an unreadable directory fails the walk");
        assert_eq!(error.slug(), errors::index::workspace_filesystem::SLUG);
        assert_eq!(error_path(&error), Some(locked.clone()));
    }

    /// A request-time capture refuses a file the process cannot read, naming it, as the
    /// build does.
    #[cfg(unix)]
    #[test]
    fn test_capture_refuses_a_file_the_process_cannot_read() {
        use std::os::unix::fs::PermissionsExt as _;

        let (directory, _) = two_file_workspace();
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let sealed = root.join("sealed.txt");
        fs::write(&sealed, "sealed\n").expect("sealed file");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).expect("remove read");
        let outcome = capture_digests_with_languages(
            &root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            &LastCapture::default(),
        );
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o644)).expect("restore read");
        let error = outcome.expect_err("a read this process cannot make fails the capture");
        assert_eq!(error.slug(), errors::index::workspace_filesystem::SLUG);
        assert_eq!(error_path(&error), Some(sealed.clone()));
    }

    #[test]
    fn test_capture_reads_again_a_path_whose_modification_time_moved() {
        let (directory, modified) = two_file_workspace();
        let root = directory.path();
        let (first, last) = captured_after(root, &LastCapture::default());
        rewrite(
            &root.join("src/lib.rs"),
            b"pub fn moved() {}\n",
            modified + std::time::Duration::from_secs(2),
        );
        let (second, next) = captured_after(root, &last);
        assert!(
            matches!(next.read_paths(), 1 | 2),
            "the edited path is read; files without boundary proof use full-read fallback"
        );
        assert_ne!(second.fingerprint(), first.fingerprint());
        let (fresh, _) = captured_after(root, &LastCapture::default());
        assert_eq!(
            second.fingerprint(),
            fresh.fingerprint(),
            "the reused capture folds to what a full read folds to"
        );
    }

    #[test]
    fn test_capture_reads_again_under_other_limits() {
        let (directory, _) = two_file_workspace();
        let root = directory.path();
        let (_, last) = captured_after(root, &LastCapture::default());
        let limits = WorkspaceIndexLimits {
            files_max: WorkspaceIndexLimits::default().files_max - 1,
            ..WorkspaceIndexLimits::default()
        };
        let (_, next) = capture_digests_with_languages(
            root,
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            &last,
        )
        .expect("the capture must read the tree");
        assert_eq!(
            next.read_paths(),
            2,
            "a record under other limits is not reused"
        );
    }

    /// A same-size rewrite that restores the modification time still moves the status
    /// change time, so the capture reads the file again and sees the rewrite.
    #[cfg(unix)]
    #[test]
    fn test_capture_reads_again_a_same_size_rewrite_that_restores_the_modification_time() {
        use std::os::unix::fs::MetadataExt as _;

        let (directory, modified) = two_file_workspace();
        let root = directory.path();
        let path = root.join("src/lib.rs");
        let (first, last) = captured_after(root, &LastCapture::default());
        if !crate::capture::supports_stat_reuse(root) {
            rewrite(&path, b"pub fn keep() {}\n", modified);
            let (second, next) = captured_after(root, &last);
            let (fresh, _) = captured_after(root, &LastCapture::default());
            assert_eq!(
                next.read_paths(),
                last.read_paths(),
                "unsupported filesystem type reads every path after restored-time rewrite"
            );
            assert_ne!(second.fingerprint(), first.fingerprint());
            assert_eq!(second.fingerprint(), fresh.fingerprint());
            return;
        }
        let recorded = fs::metadata(&path).expect("recorded stat");
        let changed = |metadata: &fs::Metadata| (metadata.ctime(), metadata.ctime_nsec());
        // The rewrite repeats until the status change clock moves, so the case under test
        // isolates a restored modification time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            rewrite(&path, b"pub fn keep() {}\n", modified);
            let current = fs::metadata(&path).expect("rewritten stat");
            if changed(&current) != changed(&recorded) {
                assert_eq!(
                    current.len(),
                    recorded.len(),
                    "the rewrite keeps the length"
                );
                assert_eq!(current.ino(), recorded.ino(), "the rewrite keeps the inode");
                assert_eq!(
                    current.modified().expect("modification time"),
                    modified,
                    "the rewrite restores the modification time"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "status change clock moved before the test deadline"
            );
            std::thread::yield_now();
        }
        let (second, next) = captured_after(root, &last);
        assert!(
            matches!(next.read_paths(), 1 | 2),
            "the moved status change time reads it; unproved paths use full-read fallback"
        );
        assert_ne!(second.fingerprint(), first.fingerprint());
        let (fresh, _) = captured_after(root, &LastCapture::default());
        assert_eq!(second.fingerprint(), fresh.fingerprint());
    }

    #[cfg(unix)]
    #[test]
    fn test_capture_repeated_restored_modification_time_rewrites_match_full_reads() {
        let (directory, modified) = two_file_workspace();
        let root = directory.path();
        let path = root.join("src/lib.rs");
        let (_, first) = captured_after(root, &LastCapture::default());
        let mut last;
        if crate::capture::supports_stat_reuse(root) {
            advance_capture_clock(root, &[path.clone(), root.join("notes.txt")]);
            let (_, first) = captured_after(root, &LastCapture::default());
            let (_, next) = captured_after(root, &first);
            assert_eq!(
                next.read_paths(),
                0,
                "restored-time rewrites start after proven digest reuse"
            );
            last = next;
        } else {
            let (_, next) = captured_after(root, &first);
            assert_eq!(
                next.read_paths(),
                first.read_paths(),
                "unsupported filesystem type reads every file"
            );
            last = next;
        }
        for (index, name) in (0..32)
            .map(|index| if index % 2 == 0 { "beta" } else { "zeta" })
            .enumerate()
        {
            let contents = format!("pub fn {name}() {{}}\n");
            rewrite(&path, contents.as_bytes(), modified);
            assert_eq!(
                fs::metadata(&path).expect("rewritten stat").len(),
                u64::try_from(contents.len()).expect("fixture length fits u64")
            );
            let (captured, next) = captured_after(root, &last);
            let (fresh, _) = captured_after(root, &LastCapture::default());
            assert_eq!(
                captured.fingerprint(),
                fresh.fingerprint(),
                "capture after rewrite {index} matches full read"
            );
            if crate::capture::supports_stat_reuse(root) {
                assert!(matches!(next.read_paths(), 1 | 2));
            } else {
                assert_eq!(
                    next.read_paths(),
                    last.read_paths(),
                    "unsupported filesystem type reads every file after rewrite {index}"
                );
            }
            last = next;
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_capture_reads_files_after_root_directory_replacement() {
        let parent = tempfile::tempdir().expect("workspace parent");
        let root = parent.path().join("workspace");
        fs::create_dir_all(root.join("src")).expect("source directory");
        fs::write(root.join("src/lib.rs"), "pub fn kept() {}\n").expect("source file");
        fs::write(root.join("notes.txt"), "plain notes\n").expect("text file");
        let (first, last) = captured_after(&root, &LastCapture::default());
        let displaced = parent.path().join("displaced");
        fs::rename(&root, &displaced).expect("move captured root aside");
        fs::create_dir(&root).expect("replace root directory");
        fs::create_dir(root.join("src")).expect("replacement source directory");
        fs::write(root.join("src/lib.rs"), "pub fn kept() {}\n").expect("replacement source");
        fs::write(root.join("notes.txt"), "plain notes\n").expect("replacement notes");

        let (second, next) = captured_after(&root, &last);
        assert_eq!(
            next.read_paths(),
            2,
            "a replacement root cannot reuse the displaced root's file records"
        );
        assert_eq!(second.fingerprint(), first.fingerprint());
    }

    #[cfg(windows)]
    #[test]
    fn test_capture_full_reads_windows_rewrites_with_writer_open()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::fs::OpenOptions;
        use std::io::{Seek, SeekFrom, Write};

        let (directory, modified) = two_file_workspace();
        let root = directory.path();
        let path = root.join("src/lib.rs");
        let mut writer = OpenOptions::new().read(true).write(true).open(&path)?;
        let (_, mut last) = captured_after(root, &LastCapture::default());
        for (index, name) in (0..32)
            .map(|index| if index % 2 == 0 { "beta" } else { "zeta" })
            .enumerate()
        {
            writer.seek(SeekFrom::Start(0))?;
            let contents = format!("pub fn {name}() {{}}\n");
            writer.write_all(contents.as_bytes())?;
            writer.set_modified(modified)?;
            assert_eq!(fs::metadata(&path)?.len(), contents.len() as u64);
            let (captured, next) = captured_after(root, &last);
            assert_eq!(
                next.read_paths(),
                2,
                "capture reads both files while writer remains open after rewrite {index}"
            );
            let (fresh, _) = captured_after(root, &LastCapture::default());
            assert_eq!(
                captured.fingerprint(),
                fresh.fingerprint(),
                "capture after rewrite {index} matches full read"
            );
            last = next;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    #[test]
    fn test_capture_full_reads_repeated_same_size_rewrites_with_restored_modification_time() {
        let (directory, modified) = two_file_workspace();
        let root = directory.path();
        let path = root.join("src/lib.rs");
        let (_, mut last) = captured_after(root, &LastCapture::default());
        for (index, name) in (0..32)
            .map(|index| if index % 2 == 0 { "beta" } else { "zeta" })
            .enumerate()
        {
            rewrite(
                &path,
                format!("pub fn {name}() {{}}\n").as_bytes(),
                modified,
            );
            let (captured, next) = captured_after(root, &last);
            assert_eq!(
                next.read_paths(),
                2,
                "platform without verified status change time reads every path at rewrite {index}"
            );
            let (fresh, _) = captured_after(root, &LastCapture::default());
            assert_eq!(
                captured.fingerprint(),
                fresh.fingerprint(),
                "capture after rewrite {index} matches a full read"
            );
            last = next;
        }
    }

    #[test]
    fn test_workspace_rebuild_stops_when_cancelled_between_changed_paths() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let directory = tempfile::tempdir().expect("workspace");
        fs::write(directory.path().join("a.rs"), "pub fn first() {}\n").expect("source");
        fs::write(directory.path().join("b.rs"), "pub fn second() {}\n").expect("source");
        let index = indexed(directory.path(), &TextFileInclusion::default());
        fs::write(directory.path().join("a.rs"), "pub fn changed_first() {}\n")
            .expect("changed source");
        fs::write(
            directory.path().join("b.rs"),
            "pub fn changed_second() {}\n",
        )
        .expect("changed source");
        let changes = resolved(&index, directory.path(), &["a.rs", "b.rs"]);
        assert_eq!(changes.len(), 2, "both named paths need replacement");
        let checks = AtomicUsize::new(0);
        // `rebuilt_cancellable` checks once before each sorted changed path. First path
        // therefore completes; cancellation refuses second path before reading it.
        let cancelled = || checks.fetch_add(1, Ordering::SeqCst) >= 1;
        let error = index
            .rebuilt_cancellable(&changes, &cancelled)
            .expect_err("cancelled rebuild must stop between changed paths");
        assert_eq!(error.slug(), errors::index::workspace_cancelled::SLUG);
        assert_eq!(
            checks.load(Ordering::SeqCst),
            2,
            "second path observes cancellation"
        );
    }

    /// A tree holding lockfiles under the default exclusion list, beside one source file.
    fn lockfile_workspace() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("web")).expect("fixture directory");
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n").expect("source");
        fs::write(root.join("Cargo.lock"), "version = 4\n").expect("lockfile");
        fs::write(root.join("deno.lock"), "{\"version\": \"4\"}\n").expect("lockfile");
        fs::write(root.join("web/package-lock.json"), "{\"name\": \"web\"}\n").expect("lockfile");
        directory
    }

    fn project(path: &str) -> ProjectPath {
        ProjectPath::new(path).expect("fixture path")
    }

    /// A lockfile the default list names is parsed by no provider and holds no text row,
    /// yet its digests stay recorded, so a capture of the tree agrees with the index.
    #[test]
    fn test_lockfiles_leave_search_and_keep_their_digests() {
        let directory = lockfile_workspace();
        let root = directory.path();
        let index = indexed(root, &TextFileInclusion::default());
        for path in ["Cargo.lock", "deno.lock", "web/package-lock.json"] {
            let path = project(path);
            assert!(
                index.file(&path).is_none(),
                "{path} is parsed by no provider"
            );
            assert!(index.text_file(&path).is_none(), "{path} holds no text row");
            assert!(index.digest(&path).is_some(), "{path} keeps its digest");
        }
        assert_eq!(
            index
                .excluded_lockfiles()
                .map(ProjectPath::as_str)
                .collect::<Vec<_>>(),
            ["Cargo.lock", "deno.lock", "web/package-lock.json"]
        );
        let documents = index.index_documents();
        assert!(
            documents
                .iter()
                .all(|document| document_path(document).as_str() == "lib.rs"),
            "only the source file publishes rows"
        );
        assert!(
            index.warnings().is_empty(),
            "an excluded lockfile warns of nothing"
        );
        let (captured, _) = capture_digests_with_languages(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            &LastCapture::default(),
        )
        .expect("the capture reads the tree");
        assert_eq!(captured.fingerprint(), *index.fingerprint());
    }

    /// A lockfile holding a NUL byte is left out as any binary file is, digest and all, and
    /// the capture of the tree agrees with the index.
    #[test]
    fn test_a_binary_lockfile_is_left_out_with_its_warning() {
        let directory = lockfile_workspace();
        let root = directory.path();
        fs::write(root.join("Cargo.lock"), "version = 4\0\n").expect("binary lockfile");
        let index = indexed(root, &TextFileInclusion::default());
        let path = project("Cargo.lock");
        assert_eq!(
            index.warnings(),
            [WorkspaceIndexWarning::BinarySource(path.clone())]
        );
        assert!(index.digest(&path).is_none());
        let (captured, _) = captured_after(root, &LastCapture::default());
        assert_eq!(captured.fingerprint(), *index.fingerprint());
    }

    /// Lockfile bytes count against `workspace_bytes_max` as a text file's do, so a
    /// lockfile past the bound refuses the build and the capture alike, naming it.
    #[test]
    fn test_a_lockfile_past_the_workspace_bound_refuses_naming_it() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n").expect("source");
        fs::write(root.join("Cargo.lock"), "version = 4\n").expect("lockfile");
        let limits = WorkspaceIndexLimits::new(8, 1_024, 24, 8, 8).expect("bounds");
        let visibility = SourceVisibility::default();
        let text = TextFileInclusion::default();
        let built = WorkspaceIndex::build(&root, limits, &visibility, &text)
            .expect_err("the lockfile passes the workspace bound");
        let languages = LanguageFileSelections::default();
        let last = LastCapture::default();
        let captured =
            capture_digests_with_languages(&root, limits, &visibility, &text, &languages, &last)
                .expect_err("the capture counts the lockfile too");
        let lockfile = root.join("Cargo.lock");
        for error in [built, captured] {
            assert_eq!(
                error.slug(),
                errors::index::workspace_workspace_too_large::SLUG
            );
            assert_eq!(error_path(&error), Some(lockfile.clone()));
        }
    }

    /// `paths.force_include` reaches a lockfile the index leaves out, and parses it when a
    /// provider claims its extension.
    #[test]
    fn test_force_include_reaches_a_parsed_lockfile() {
        let directory = lockfile_workspace();
        let index = indexed(directory.path(), &TextFileInclusion::default());
        let forced = index
            .force_include_index(&["web/package-lock.json".to_owned()], 8)
            .expect("the force-included lockfile reads");
        let file = forced
            .file(&project("web/package-lock.json"))
            .expect("the JSON provider parses the lockfile");
        assert!(
            !file.syntax().symbols().is_empty(),
            "its keys are declarations"
        );
    }

    /// An empty exclusion list indexes every lockfile again.
    #[test]
    fn test_an_empty_exclusion_list_indexes_lockfiles() {
        let directory = lockfile_workspace();
        let inclusion = TextFileInclusion::default().excluding_lockfiles(Vec::new());
        let index = indexed(directory.path(), &inclusion);
        assert!(index.text_file(&project("Cargo.lock")).is_some());
        assert!(index.file(&project("web/package-lock.json")).is_some());
        assert_eq!(index.excluded_lockfiles().count(), 0);
    }

    /// An edit to an excluded lockfile moves the workspace: the rebuild over that one path
    /// records its new digest and still stores no row for it.
    #[test]
    fn test_an_edited_lockfile_moves_the_workspace_and_stays_out_of_search() {
        let directory = lockfile_workspace();
        let root = directory.path();
        let index = indexed(root, &TextFileInclusion::default());
        let before = index.digest(&project("Cargo.lock"));
        fs::write(root.join("Cargo.lock"), "version = 4\n\n[[package]]\n").expect("edit");
        let (captured, _) = capture_digests_with_languages(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            &LastCapture::default(),
        )
        .expect("the capture reads the tree");
        let changes = PathChanges::between(&index.digests(), &captured);
        assert_eq!(
            changes.paths().map(ProjectPath::as_str).collect::<Vec<_>>(),
            ["Cargo.lock"]
        );
        let rebuilt = index
            .rebuilt(&changes)
            .expect("the rebuild reads the lockfile");
        assert_ne!(rebuilt.digest(&project("Cargo.lock")), before);
        assert!(rebuilt.text_file(&project("Cargo.lock")).is_none());
        assert_eq!(captured.fingerprint(), *rebuilt.fingerprint());
        assert!(rebuilt.index_documents_for(changes.paths()).is_empty());
    }

    /// A text file past the chunk bound publishes each chunk at its start in the file, and a
    /// file within the bound publishes itself at offset zero.
    #[test]
    fn test_text_rows_record_where_their_text_starts_in_the_file() {
        let directory = tempfile::tempdir().expect("workspace");
        let text = "one line of chunked text\n".repeat(120);
        fs::write(directory.path().join("big.txt"), &text).expect("big text");
        fs::write(directory.path().join("small.txt"), "small\n").expect("small text");
        let index = indexed(
            directory.path(),
            &TextFileInclusion::new(vec!["**".to_owned()], 1_024),
        );
        let documents = index.index_documents();
        let small = documents
            .iter()
            .find(|document| document.identity().as_str() == "small.txt")
            .expect("the small file publishes one row");
        assert_eq!(small.byte_offset(), Some(0));
        let chunks: Vec<&IndexDocument> = documents
            .iter()
            .filter(|document| document.identity().as_str().starts_with("big.txt#"))
            .collect();
        assert!(chunks.len() > 2, "the big file splits: {}", chunks.len());
        for chunk in chunks {
            let start = usize::try_from(chunk.byte_offset().expect("a chunk records its start"))
                .expect("offset fits");
            assert_eq!(&text[start..start + chunk.content().len()], chunk.content());
        }
    }

    #[test]
    fn test_a_capture_folds_the_tree_revision_the_build_stamps() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        fs::create_dir_all(root.join("src")).expect("fixture directory");
        fs::write(root.join("src/lib.rs"), "pub fn beacon() {}\n").expect("source");
        fs::write(root.join("src/main.rs"), "pub fn lantern() {}\n").expect("source");
        fs::write(root.join("NOTES.txt"), "notes\n").expect("text");
        let inclusion = TextFileInclusion::default();
        let index = indexed(root, &inclusion);
        let capture = |root: &Path| {
            capture_digests_with_languages(
                root,
                WorkspaceIndexLimits::default(),
                &SourceVisibility::default(),
                &inclusion,
                &LanguageFileSelections::default(),
                &LastCapture::default(),
            )
            .expect("the capture must read the tree")
            .0
        };
        let captured = capture(root);
        assert_eq!(
            captured.tree_revision(),
            Some(index.tree_revision().as_str())
        );
        assert_eq!(
            index.tree_revision().len(),
            64,
            "the build keeps the full hash"
        );
        assert_eq!(captured.fingerprint(), *index.fingerprint());

        fs::write(root.join("NOTES.txt"), "edited notes\n").expect("text");
        let text_moved = capture(root);
        assert_eq!(
            text_moved.tree_revision(),
            Some(index.tree_revision().as_str()),
            "a text file's bytes never enter the tree revision"
        );
        assert_ne!(text_moved.fingerprint(), *index.fingerprint());

        fs::write(
            root.join("src/lib.rs"),
            "pub fn beacon() {}\npub fn torch() {}\n",
        )
        .expect("source");
        let source_moved = capture(root);
        assert_ne!(
            source_moved.tree_revision(),
            Some(index.tree_revision().as_str()),
            "a syntax-indexed file's bytes move the tree revision"
        );
    }
}
