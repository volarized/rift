use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, Match, Walk, WalkBuilder};
use rift_core::constants::{
    READ_RESULTS_MAX_DEFAULT, VCS_IGNORE_FILE, WORKSPACE_BYTES_MAX_DEFAULT,
    WORKSPACE_CONFIGURATION_FILE, WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT,
    WORKSPACE_FILES_MAX_DEFAULT, WORKSPACE_IGNORED_DIRECTORIES,
};
use rift_core::{
    CompositionId, Error, ErrorCode, ErrorContext, ErrorName, Fault, LanguageFileSelections,
    PortableSymbolFacts, ProjectPath, ProviderId, SourceVisibility, SymbolId, TextFileInclusion,
    fault_label, symbol_identity,
};
use rift_provider::{
    AssembledSymbol, Component, CompositionBuilder, NormalizedGraph, ProviderComposition,
};
use rift_syntax::{
    SyntaxDocument, SyntaxError, SyntaxNode, SyntaxProvider, SyntaxSource, SyntaxSymbol, registry,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::change_set::{FileDigest, PathChanges, WorkspaceDigests};
use crate::chunk::text_chunks;
use crate::glob::PathMatcher;
use crate::language::{ClassifiedPath, LanguagePolicyError, WorkspaceLanguagePolicy};
use crate::lexical::{LexicalUnit, LexicalUnitKind};
use crate::semantic::WorkspaceSemantics;

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
    directory_depth_max: usize,
    results_max: usize,
}

impl WorkspaceIndexLimits {
    /// Constructs positive direct-workspace bounds.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when any bound is zero.
    pub fn new(
        files_max: usize,
        file_bytes_max: usize,
        workspace_bytes_max: usize,
        directory_depth_max: usize,
        results_max: usize,
    ) -> Result<Self, WorkspaceIndexError> {
        let limits = Self {
            files_max,
            file_bytes_max,
            workspace_bytes_max,
            directory_depth_max,
            results_max,
        };
        for bound in limits.bounds() {
            positive_bound(bound)?;
        }
        Ok(limits)
    }

    const fn bounds(self) -> [usize; 5] {
        [
            self.files_max,
            self.file_bytes_max,
            self.workspace_bytes_max,
            self.directory_depth_max,
            self.results_max,
        ]
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
}

impl Default for WorkspaceIndexLimits {
    fn default() -> Self {
        Self {
            files_max: WORKSPACE_FILES_MAX_DEFAULT,
            file_bytes_max: registry::file_bytes_max_default(),
            workspace_bytes_max: WORKSPACE_BYTES_MAX_DEFAULT,
            directory_depth_max: WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT,
            results_max: READ_RESULTS_MAX_DEFAULT,
        }
    }
}

/// Stable workspace indexing failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceIndexViolation {
    /// Limit was configured as zero.
    ZeroLimit,
    /// Root cannot be canonicalized or is not directory.
    InvalidRoot,
    /// Directory depth exceeds bound.
    TooDeep,
    /// Rust source count exceeds bound.
    TooManyFiles,
    /// One Rust source exceeds byte bound.
    FileTooLarge,
    /// Aggregate Rust source bytes exceed bound.
    WorkspaceTooLarge,
    /// Workspace path is not UTF-8 or canonical project syntax.
    InvalidPath,
    /// An included file's bytes are not valid UTF-8: a Rust source file or a `[search.text]`
    /// text file.
    InvalidSource,
    /// Filesystem operation failed.
    Filesystem,
    /// Rust syntax analysis failed.
    Syntax,
    /// Provider publication or normalization failed.
    Provider,
    /// Composition recipe failed validation.
    Composition,
    /// Requested result bound exceeds configured maximum.
    ResultLimit,
    /// A `source.include` or `source.exclude` entry is not a valid glob.
    SourcePatternInvalid,
    /// An unshipped language has no nonempty include list.
    LanguageIncludeRequired,
    /// One visible path matches two language entries.
    LanguageMatchConflict,
    /// The workspace's version-control repository could not serve the tree.
    History,
}

/// One workspace indexing failure: its violation, the offending path when
/// known, and the underlying cause (I/O, UTF-8, syntax).
#[derive(Debug)]
pub struct WorkspaceIndexFault {
    violation: WorkspaceIndexViolation,
    path: Option<PathBuf>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl WorkspaceIndexFault {
    /// Returns stable failure classification.
    #[must_use]
    pub const fn violation(&self) -> WorkspaceIndexViolation {
        self.violation
    }

    /// The version-control failure behind a `History` violation, which owns
    /// this fault's registry identity and evidence.
    fn history_source(&self) -> Option<&rift_history::HistoryError> {
        if self.violation != WorkspaceIndexViolation::History {
            return None;
        }
        self.source
            .as_deref()
            .and_then(|source| source.downcast_ref::<rift_history::HistoryError>())
    }

    /// Returns involved filesystem path when available.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl Fault for WorkspaceIndexFault {
    /// A syntax failure delegates to the underlying [`SyntaxError`]'s
    /// identity when the source downcasts to one.
    fn name(&self) -> ErrorName {
        match self.violation {
            WorkspaceIndexViolation::ZeroLimit
            | WorkspaceIndexViolation::InvalidRoot
            | WorkspaceIndexViolation::Composition
            | WorkspaceIndexViolation::SourcePatternInvalid
            | WorkspaceIndexViolation::LanguageIncludeRequired
            | WorkspaceIndexViolation::LanguageMatchConflict => {
                ErrorName::Wire(ErrorCode::ConfigurationInvalid)
            }
            WorkspaceIndexViolation::TooDeep
            | WorkspaceIndexViolation::TooManyFiles
            | WorkspaceIndexViolation::FileTooLarge
            | WorkspaceIndexViolation::WorkspaceTooLarge
            | WorkspaceIndexViolation::ResultLimit => ErrorName::Wire(ErrorCode::LimitExceeded),
            WorkspaceIndexViolation::InvalidPath => ErrorName::Wire(ErrorCode::UnsupportedPath),
            WorkspaceIndexViolation::InvalidSource => {
                ErrorName::Wire(ErrorCode::ContentUnavailable)
            }
            WorkspaceIndexViolation::Filesystem => ErrorName::Wire(ErrorCode::StorageFailure),
            WorkspaceIndexViolation::Syntax => self
                .source
                .as_deref()
                .and_then(|source| source.downcast_ref::<SyntaxError>())
                .map_or_else(
                    || ErrorName::Wire(ErrorCode::InternalError),
                    |error| error.descriptor().name(),
                ),
            WorkspaceIndexViolation::Provider => ErrorName::Wire(ErrorCode::InternalError),
            WorkspaceIndexViolation::History => self.history_source().map_or_else(
                || ErrorName::Wire(ErrorCode::InternalError),
                |error| error.descriptor().name(),
            ),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("violation", fault_label(&self.violation))];
        if let Some(path) = &self.path {
            context.push(ErrorContext::new("path", path.display().to_string()));
        }
        if let Some(error) = self.history_source() {
            context.extend(error.context());
        }
        if let Some(error) = self
            .source
            .as_deref()
            .and_then(|source| source.downcast_ref::<LanguagePolicyError>())
        {
            context.extend(
                error
                    .evidence()
                    .into_iter()
                    .map(|(key, value)| ErrorContext::new(key, value)),
            );
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// Opaque workspace indexing failure.
pub type WorkspaceIndexError = Error<WorkspaceIndexFault>;

pub(crate) fn index_error(violation: WorkspaceIndexViolation) -> WorkspaceIndexError {
    Error::new(WorkspaceIndexFault {
        violation,
        path: None,
        source: None,
    })
}

pub(crate) fn index_error_at(
    violation: WorkspaceIndexViolation,
    path: &Path,
) -> WorkspaceIndexError {
    Error::new(WorkspaceIndexFault {
        violation,
        path: Some(path.to_path_buf()),
        source: None,
    })
}

pub(crate) fn index_error_caused_by(
    violation: WorkspaceIndexViolation,
    path: Option<&Path>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> WorkspaceIndexError {
    Error::new(WorkspaceIndexFault {
        violation,
        path: path.map(Path::to_path_buf),
        source: Some(Box::new(source)),
    })
}

/// One immutable file enriched with syntax facts.
///
/// `Eq` is not derived: `syntax` carries [`SyntaxDocument`], which is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedFile {
    path: ProjectPath,
    source: String,
    digest: FileDigest,
    executable: bool,
    syntax: SyntaxDocument,
}

impl IndexedFile {
    /// Returns project-relative path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns complete UTF-8 source.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the digest of the bytes this file was indexed from.
    ///
    /// The digest is taken from the bytes the index actually read, so a publication's
    /// digests always name the publication's own source - a file that moved again while
    /// the index was building is caught by the next observation instead.
    #[must_use]
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }

    /// Whether this file was executable when the index captured it.
    #[must_use]
    pub const fn executable(&self) -> bool {
        self.executable
    }

    /// Returns syntax facts.
    #[must_use]
    pub const fn syntax(&self) -> &SyntaxDocument {
        &self.syntax
    }
}

/// One immutable visible UTF-8 file in the baseline content catalog.
///
/// A file over `[search.text].max_chunk` lands here whole. Search unit
/// derivation splits its content while keeping one file identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSourceFile {
    path: ProjectPath,
    content: String,
    digest: FileDigest,
    executable: bool,
}

impl TextSourceFile {
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
    pub rank: SymbolMatchRank,
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

/// Stable semantic priority for symbol-name matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SymbolMatchRank {
    /// Query equals complete qualified name.
    QualifiedExact,
    /// Query equals short declaration name.
    NameExact,
    /// Short declaration name starts with query.
    NamePrefix,
    /// Complete qualified name contains query elsewhere.
    Substring,
}

/// Exact identity of visible workspace source paths and bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFingerprint([u8; 32]);

/// Compiled source visibility used by filesystem event inclusion.
#[derive(Debug)]
pub struct WorkspaceSourcePolicy {
    root: PathBuf,
    watched_root: PathBuf,
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
    /// Returns [`WorkspaceIndexError`] for discovery, read, or configured-bound failures.
    pub fn capture(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
    ) -> Result<Self, WorkspaceIndexError> {
        Ok(capture_digests(root, limits, visibility)?.fingerprint())
    }

    /// Folds one publication's own files, absorbing each file's digest rather than its
    /// bytes. Files already carry the digest of what the index read, so this costs one
    /// hash update per file however large the workspace's sources are.
    fn from_files(
        files: &BTreeMap<ProjectPath, Arc<IndexedFile>>,
        text_files: &BTreeMap<ProjectPath, Arc<TextSourceFile>>,
    ) -> Self {
        Self::from_digests(&keyed_digests(files, text_files))
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
    /// Returns [`WorkspaceIndexError`] for invalid roots, patterns, ignore files,
    /// or configured-bound failures.
    pub fn build(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
    ) -> Result<Self, WorkspaceIndexError> {
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
    /// Returns [`WorkspaceIndexError`] for invalid roots, patterns, ignore files,
    /// or configured-bound failures.
    pub fn build_with_languages(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
    ) -> Result<Self, WorkspaceIndexError> {
        let watched_root = root.to_path_buf();
        let root = canonical_root(root)?;
        let matcher = PathMatcher::build(&root, visibility.include(), visibility.exclude())?;
        let language = WorkspaceLanguagePolicy::build(&root, languages, text_inclusion)?;
        let gitignore = visibility
            .respect_gitignore()
            .then(|| GitignoreChain::build(&root, limits))
            .transpose()?;
        Ok(Self {
            root,
            watched_root,
            matcher,
            gitignore,
            language,
        })
    }

    /// Whether `path` is visible to the workspace: above the hard floor, kept by the
    /// `[source]` policy, and not excluded by the workspace's `.gitignore` chain.
    /// Visibility is what the change tools reach. The index narrows it further to the
    /// extensions a syntax provider or the text policy claims - see [`Self::includes`].
    #[must_use]
    pub fn visible(&self, path: &Path) -> bool {
        let Some(path) = self.normalized_path(path) else {
            return false;
        };
        self.visible_normalized(path.as_ref())
    }

    /// Returns whether one path passes current workspace visibility policy.
    #[must_use]
    pub fn includes(&self, path: &Path) -> bool {
        let Some(path) = self.normalized_path(path) else {
            return false;
        };
        self.visible_normalized(path.as_ref())
            && self
                .language
                .classifies(path.as_ref())
                .map_or(true, |selection| selection.is_some())
    }

    /// Effective language entry matching one path after visibility accepts it.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when two language entries match.
    pub fn language_for_path(
        &self,
        path: &Path,
    ) -> Result<Option<&crate::EffectiveLanguage>, WorkspaceIndexError> {
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

    /// Carries the checks [`Self::visible`] and [`Self::includes`] share, against a path
    /// [`Self::normalized_path`] already resolved: above the hard floor, kept by the
    /// `[source]` matcher, and not excluded by the workspace's `.gitignore` chain. Neither
    /// caller normalizes twice.
    fn visible_normalized(&self, path: &Path) -> bool {
        let above_hard_floor = hard_floor_includes_path(&self.root, path);
        if !above_hard_floor {
            return false;
        }
        let configuration_includes = self.matcher.includes(path);
        let gitignore_includes = self
            .gitignore
            .as_ref()
            .is_none_or(|gitignore| !gitignore.excludes(path, false));
        configuration_includes && gitignore_includes
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
        let configuration_includes = self.matcher.may_include_descendant(path);
        let gitignore_includes = self
            .gitignore
            .as_ref()
            .is_none_or(|gitignore| !gitignore.excludes(path, true));
        configuration_includes && gitignore_includes
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
        let Some(normalized) = self.normalized_path(path) else {
            return false;
        };
        let normalized = normalized.as_ref();
        if normalized == self.root.join(WORKSPACE_CONFIGURATION_FILE) {
            return true;
        }
        normalized.file_name() == Some(OsStr::new(VCS_IGNORE_FILE))
            && normalized
                .parent()
                .is_some_and(|parent| self.may_include_descendant(parent))
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

/// One file omitted from the baseline content catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceIndexWarning {
    /// File bytes are not valid UTF-8.
    InvalidUtf8Source(ProjectPath),
    /// File bytes contain a NUL byte.
    BinarySource(ProjectPath),
    /// File exceeds the configured per-file byte bound.
    FileTooLarge(ProjectPath),
}

/// Outcome of reading one baseline catalog candidate.
enum CatalogRead {
    Included(TextSourceFile),
    Skipped(WorkspaceIndexWarning),
}

impl WorkspaceIndexWarning {
    /// The file this warning names.
    #[must_use]
    pub fn path(&self) -> &ProjectPath {
        match self {
            Self::InvalidUtf8Source(path) | Self::BinarySource(path) | Self::FileTooLarge(path) => {
                path
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
    composition: ProviderComposition,
    limits: WorkspaceIndexLimits,
    language: Arc<WorkspaceLanguagePolicy>,
    text_inclusion: TextFileInclusion,
    fingerprint: WorkspaceFingerprint,
    semantics: WorkspaceSemantics,
    warnings: Vec<WorkspaceIndexWarning>,
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
    /// Returns `WorkspaceIndexError` for invalid root, I/O, syntax, invalid
    /// source pattern, or exceeded workspace bound.
    pub fn build(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
    ) -> Result<Self, WorkspaceIndexError> {
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
    /// Returns [`WorkspaceIndexError`] for invalid configuration, I/O, syntax,
    /// or exceeded workspace bounds.
    pub fn build_with_languages(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
    ) -> Result<Self, WorkspaceIndexError> {
        let root = canonical_root(root)?;
        let composition = composition()?;
        let language = Arc::new(WorkspaceLanguagePolicy::build(
            &root,
            languages,
            text_inclusion,
        )?);
        let classified = discover(&root, limits, visibility, &language)?;
        let mut workspace_bytes = 0_usize;
        let mut warnings = Vec::new();
        let mut files = BTreeMap::new();
        let mut text_files = BTreeMap::new();
        for path in classified.source {
            let Some(ClassifiedPath::Source(provider)) = language.classifies(&path)? else {
                continue;
            };
            match read_catalog_file(&root, &path, limits, &mut workspace_bytes)? {
                CatalogRead::Included(text_file) => {
                    let file = indexed_file_from_catalog(&text_file, &path, provider)?;
                    files.insert(file.path().clone(), Arc::new(file));
                    text_files.insert(text_file.path().clone(), Arc::new(text_file));
                }
                CatalogRead::Skipped(warning) => warnings.push(warning),
            }
        }
        for path in classified.text {
            match read_catalog_file(&root, &path, limits, &mut workspace_bytes)? {
                CatalogRead::Included(file) => {
                    text_files.insert(file.path().clone(), Arc::new(file));
                }
                CatalogRead::Skipped(warning) => warnings.push(warning),
            }
        }
        warnings.sort_by(|left, right| left.path().cmp(right.path()));
        let fingerprint = WorkspaceFingerprint::from_files(&files, &text_files);
        let semantics = WorkspaceSemantics::build(
            files.values().map(|file| file.syntax()),
            fingerprint.revision_number(),
            None,
        )
        .map_err(provider_error)?;
        Ok(Self {
            root,
            files,
            text_files,
            composition,
            limits,
            language,
            text_inclusion: text_inclusion.clone(),
            fingerprint,
            semantics,
            warnings,
        })
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
    /// Returns [`WorkspaceIndexError`] for I/O, syntax, or an exceeded bound, exactly as a
    /// whole scan does. A named path the filesystem no longer holds is dropped rather than
    /// refused: the observation that named it has already been superseded by the deletion.
    /// A named path whose bytes are not UTF-8 is dropped the same way a whole scan drops
    /// one, and a warning naming it replaces any warning the previous index carried for
    /// that path.
    pub fn rebuilt(&self, changes: &PathChanges) -> Result<Self, WorkspaceIndexError> {
        let mut files = self.files.clone();
        let mut text_files = self.text_files.clone();
        for path in changes.paths() {
            files.remove(path);
            text_files.remove(path);
        }
        let mut workspace_bytes = Self::indexed_bytes(&files, &text_files);
        let touched: BTreeSet<&ProjectPath> = changes.paths().collect();
        let mut warnings: Vec<WorkspaceIndexWarning> = self
            .warnings
            .iter()
            .filter(|warning| !touched.contains(warning.path()))
            .cloned()
            .collect();
        for path in changes.indexed() {
            self.read_indexed_path(
                path,
                &mut files,
                &mut text_files,
                &mut workspace_bytes,
                &mut warnings,
            )?;
        }
        warnings.sort_by(|left, right| left.path().cmp(right.path()));
        let fingerprint = WorkspaceFingerprint::from_files(&files, &text_files);
        let semantics = WorkspaceSemantics::build(
            files.values().map(|file| file.syntax()),
            fingerprint.revision_number(),
            Some(self.semantics.graph()),
        )
        .map_err(provider_error)?;
        Ok(Self {
            root: self.root.clone(),
            files,
            text_files,
            composition: composition()?,
            limits: self.limits,
            language: Arc::clone(&self.language),
            text_inclusion: self.text_inclusion.clone(),
            fingerprint,
            semantics,
            warnings,
        })
    }

    /// Reads one changed visible path into syntax facts and baseline content catalog.
    fn read_indexed_path(
        &self,
        path: &ProjectPath,
        files: &mut BTreeMap<ProjectPath, Arc<IndexedFile>>,
        text_files: &mut BTreeMap<ProjectPath, Arc<TextSourceFile>>,
        workspace_bytes: &mut usize,
        warnings: &mut Vec<WorkspaceIndexWarning>,
    ) -> Result<(), WorkspaceIndexError> {
        let absolute = self.root.join(path.as_str());
        if !absolute.is_file() {
            return Ok(());
        }
        let Some(class) = self.language.classifies(&absolute)? else {
            return Ok(());
        };
        match read_catalog_file(&self.root, &absolute, self.limits, workspace_bytes)? {
            CatalogRead::Included(text_file) => {
                if let ClassifiedPath::Source(provider) = class {
                    let indexed_file = indexed_file_from_catalog(&text_file, &absolute, provider)?;
                    files.insert(indexed_file.path().clone(), Arc::new(indexed_file));
                }
                text_files.insert(text_file.path().clone(), Arc::new(text_file));
            }
            CatalogRead::Skipped(warning) => warnings.push(warning),
        }
        Ok(())
    }

    /// Bytes shared files already contribute to workspace byte bound.
    fn indexed_bytes(
        files: &BTreeMap<ProjectPath, Arc<IndexedFile>>,
        text_files: &BTreeMap<ProjectPath, Arc<TextSourceFile>>,
    ) -> usize {
        let catalog: usize = text_files.values().map(|file| file.content().len()).sum();
        let syntax_only: usize = files
            .iter()
            .filter(|(path, _)| !text_files.contains_key(*path))
            .map(|(_, file)| file.source().len())
            .sum();
        catalog.saturating_add(syntax_only)
    }

    /// Assembles an index from files another source already accepted - the
    /// revision build, whose bytes come from git objects instead of a
    /// directory walk. Revision reads carry no text files: `text_inclusion` is kept only
    /// so a future revision-text feature can reuse this constructor unchanged.
    pub(crate) fn from_parts(
        root: PathBuf,
        files: Vec<IndexedFile>,
        text_files: Vec<TextSourceFile>,
        composition: ProviderComposition,
        limits: WorkspaceIndexLimits,
        language: Arc<WorkspaceLanguagePolicy>,
        text_inclusion: TextFileInclusion,
    ) -> Result<Self, WorkspaceIndexError> {
        let files = keyed_by_path(files, IndexedFile::path);
        let text_files = keyed_by_path(text_files, TextSourceFile::path);
        let fingerprint = WorkspaceFingerprint::from_files(&files, &text_files);
        let semantics = WorkspaceSemantics::build(
            files.values().map(|file| file.syntax()),
            fingerprint.revision_number(),
            None,
        )
        .map_err(provider_error)?;
        Ok(Self {
            root,
            files,
            text_files,
            composition,
            limits,
            language,
            text_inclusion,
            fingerprint,
            semantics,
            warnings: Vec::new(),
        })
    }

    /// Returns canonical real workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
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

    /// Every indexed file's digest, in project-path order.
    ///
    /// A request that captured the tree itself compares its capture with this to name the
    /// files that moved.
    #[must_use]
    pub fn digests(&self) -> WorkspaceDigests {
        keyed_digests(&self.files, &self.text_files)
    }

    /// Returns the digest of the bytes indexed at `path`, whichever class holds it.
    ///
    /// This is what resolves an observation into a change set: the caller hashes the
    /// path's current bytes and compares them with what this publication indexed.
    #[must_use]
    pub fn digest(&self, path: &ProjectPath) -> Option<FileDigest> {
        self.files
            .get(path)
            .map(|file| file.digest())
            .or_else(|| self.text_files.get(path).map(|file| file.digest()))
    }

    /// Derives lexical search units from this index: one unit per indexed symbol, carrying
    /// its declaration source, and one or more units per baseline text file - one whole unit
    /// when the file is within `[search.text].max_chunk`, one unit per chunk otherwise. A
    /// chunked file's units share its real path and share an identity built from that path
    /// plus the chunk index, so a hit still maps back to the file it came from.
    ///
    /// `force_include` files stay outside this derivation: that on-demand walk's contract
    /// covers source units read for one request, not the persistent lexical index.
    #[must_use]
    pub fn lexical_units(&self) -> Vec<LexicalUnit> {
        let mut units = Vec::with_capacity(self.files.len() + self.text_files.len());
        for file in self.files() {
            for symbol in file.syntax().symbols() {
                units.push(symbol_lexical_unit(file, symbol));
            }
        }
        for file in self.text_files() {
            push_text_lexical_units(&mut units, file, self.text_chunk_bytes_max());
        }
        units
    }

    /// Derives lexical units for the named paths alone, in the same shapes
    /// [`Self::lexical_units`] derives for the whole index.
    ///
    /// A path this index no longer holds contributes nothing, which is what a removed file
    /// owes: its stored units are deleted by path rather than replaced.
    #[must_use]
    pub fn lexical_units_for<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a ProjectPath>,
    ) -> Vec<LexicalUnit> {
        let mut units = Vec::new();
        for path in paths {
            if let Some(file) = self.files.get(path) {
                for symbol in file.syntax().symbols() {
                    units.push(symbol_lexical_unit(file, symbol));
                }
            }
            if let Some(file) = self.text_files.get(path) {
                push_text_lexical_units(&mut units, file, self.text_chunk_bytes_max());
            }
        }
        units
    }

    /// Returns each text file split into more than one lexical chunk, paired with its chunk
    /// count, so a caller can report the split instead of the index silently absorbing it.
    #[must_use]
    pub fn chunked_text_files(&self) -> Vec<(ProjectPath, usize)> {
        self.text_files()
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

    /// Assembles readable symbol through its normalized record.
    ///
    /// # Errors
    ///
    /// Returns `WorkspaceIndexError` when normalized Contributions do not
    /// supply required portable facts.
    pub fn assembled_symbol(
        &self,
        matched: SymbolMatch<'_>,
    ) -> Result<ReadableSymbol, WorkspaceIndexError> {
        let identity = symbol_identity(
            &matched.file.syntax().language().identity_segment(),
            matched.file.path().as_str(),
            &matched.symbol.qualified_name,
        );
        self.semantics
            .assembled(&identity)
            .and_then(ReadableSymbol::new)
            .ok_or_else(|| provider_error(ReadableSymbolMissing { identity }))
    }

    /// Files omitted from baseline content catalog, in project-path order.
    ///
    /// Build and rebuild continue after invalid UTF-8, NUL bytes, or a file beyond
    /// configured per-file byte bound.
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
    /// Returns [`WorkspaceIndexError`] when limit exceeds configured maximum.
    pub fn symbols(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SymbolMatch<'_>>, WorkspaceIndexError> {
        self.validate_result_limit(limit)?;
        Ok(symbol_matches(self.files(), query, limit))
    }

    /// Finds lexical source lines containing query.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when limit exceeds configured maximum.
    pub fn source_matches(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(&IndexedFile, usize, String)>, WorkspaceIndexError> {
        self.validate_result_limit(limit)?;
        Ok(source_line_matches(self.files(), query, limit))
    }

    /// Finds lexical content lines containing `query` across included `[search.text]` files -
    /// the same content-line search [`Self::source_matches`] runs over syntax-indexed files,
    /// reaching a text-lane file's bytes directly rather than only through the semantic tier.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when limit exceeds configured maximum.
    pub fn text_matches(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(&TextSourceFile, usize, String)>, WorkspaceIndexError> {
        self.validate_result_limit(limit)?;
        Ok(text_line_matches(self.text_files(), query, limit))
    }

    /// Returns file by canonical project path.
    #[must_use]
    pub fn file(&self, path: &ProjectPath) -> Option<&IndexedFile> {
        self.files.get(path).map(AsRef::as_ref)
    }

    /// Returns one baseline text file by canonical project path.
    #[must_use]
    pub fn text_file(&self, path: &ProjectPath) -> Option<&TextSourceFile> {
        self.text_files.get(path).map(AsRef::as_ref)
    }

    /// Chunk bound applied to baseline text when lexical units are derived.
    fn text_chunk_bytes_max(&self) -> u64 {
        self.text_inclusion.chunk_bytes_max()
    }

    /// Returns syntax nodes covering byte position.
    #[must_use]
    pub fn nodes(&self, path: &ProjectPath, position: u64) -> Option<Vec<&SyntaxNode>> {
        self.file(path).map(|file| file.syntax().nodes_at(position))
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
    /// Returns [`WorkspaceIndexError`] for an invalid glob, an unreadable path, invalid UTF-8
    /// source, a syntax failure, or a file exceeding this index's per-file or aggregate byte
    /// bound, or `files_max` matches.
    pub fn force_include_files(
        &self,
        force_include: &[String],
        files_max: usize,
    ) -> Result<Vec<IndexedFile>, WorkspaceIndexError> {
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
                    return Err(index_error_at(
                        WorkspaceIndexViolation::TooDeep,
                        entry.path(),
                    ));
                }
                continue;
            }
            if !file_type.is_some_and(|file_type| file_type.is_file()) {
                continue;
            }
            let path = entry.path();
            if !has_source_extension(path) {
                continue;
            }
            if !matcher.includes(path) {
                continue;
            }
            let relative = path.strip_prefix(&self.root).map_err(|error| {
                index_error_caused_by(WorkspaceIndexViolation::InvalidPath, Some(path), error)
            })?;
            let project_path = relative_path(relative)?;
            if self.file(&project_path).is_some() {
                continue;
            }
            if files.len() >= files_max {
                return Err(index_error_at(WorkspaceIndexViolation::TooManyFiles, path));
            }
            files.push(read_file(
                &self.root,
                path,
                syntax_provider_for(path),
                self.limits,
                &mut extra_bytes,
            )?);
        }
        Ok(files)
    }

    /// Walks request-selected visible files into baseline content catalog.
    fn force_include_text_files(
        &self,
        force_include: &[String],
        files_max: usize,
    ) -> Result<Vec<TextSourceFile>, WorkspaceIndexError> {
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
                    return Err(index_error_at(
                        WorkspaceIndexViolation::TooDeep,
                        entry.path(),
                    ));
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
            if files.len() >= files_max {
                return Err(index_error_at(WorkspaceIndexViolation::TooManyFiles, path));
            }
            if let CatalogRead::Included(file) =
                read_catalog_file(&self.root, path, self.limits, &mut extra_bytes)?
            {
                files.push(file);
            }
        }
        Ok(files)
    }

    /// Builds one request index over files selected by force include.
    ///
    /// Each selected path is read once into baseline content catalog. Paths with a syntax
    /// provider also receive syntax facts from those same bytes.
    ///
    /// # Errors
    ///
    /// Returns `WorkspaceIndexError` when selection, parsing, or bounds fail.
    pub fn force_include_index(
        &self,
        force_include: &[String],
        files_max: usize,
    ) -> Result<Self, WorkspaceIndexError> {
        let text_files = self.force_include_text_files(force_include, files_max)?;
        let files = text_files
            .iter()
            .filter_map(|file| {
                let context_path = self.root.join(file.path().as_str());
                has_source_extension(&context_path).then(|| {
                    indexed_file_from_catalog(
                        file,
                        &context_path,
                        syntax_provider_for(&context_path),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_parts(
            self.root.clone(),
            files,
            text_files,
            self.composition.clone(),
            self.limits,
            Arc::clone(&self.language),
            self.text_inclusion.clone(),
        )
    }

    fn validate_result_limit(&self, limit: usize) -> Result<(), WorkspaceIndexError> {
        if limit == 0 || limit > self.limits.results_max {
            return Err(index_error(WorkspaceIndexViolation::ResultLimit));
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
        .filter(|(_, symbol)| symbol.qualified_name.to_lowercase().contains(&query))
        .map(|(file, symbol)| SymbolMatch {
            file,
            symbol,
            rank: symbol_rank(symbol, &query),
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

fn positive_bound(bound: usize) -> Result<(), WorkspaceIndexError> {
    if bound == 0 {
        return Err(index_error(WorkspaceIndexViolation::ZeroLimit));
    }
    Ok(())
}

fn canonical_root(root: &Path) -> Result<PathBuf, WorkspaceIndexError> {
    let canonical = fs::canonicalize(root).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::InvalidRoot, Some(root), error)
    })?;
    if !canonical.is_dir() {
        return Err(index_error_at(
            WorkspaceIndexViolation::InvalidRoot,
            &canonical,
        ));
    }
    Ok(canonical)
}

fn composition() -> Result<ProviderComposition, WorkspaceIndexError> {
    let source = component::<(), WorkspaceFiles>("workspace-source")?;
    let syntax = component::<WorkspaceFiles, RustFacts>("rust-tree-sitter")?;
    let index = component::<RustFacts, ReadIndex>("memory-index")?;
    let mut builder =
        CompositionBuilder::new(CompositionId::new("rust-read").map_err(composition_error)?);
    let files = builder.source("project", &source);
    let facts = builder.then(files, "syntax", &syntax);
    let reads = builder.then(facts, "index", &index);
    builder.output(reads).build().map_err(composition_error)
}

pub(crate) fn component<Input: 'static, Output: 'static>(
    id: &str,
) -> Result<Component<Input, Output>, WorkspaceIndexError> {
    Ok(Component::new(
        ProviderId::new(id).map_err(composition_error)?,
    ))
}

pub(crate) fn composition_error(
    source: impl std::error::Error + Send + Sync + 'static,
) -> WorkspaceIndexError {
    index_error_caused_by(WorkspaceIndexViolation::Composition, None, source)
}

fn provider_error(source: impl std::error::Error + Send + Sync + 'static) -> WorkspaceIndexError {
    index_error_caused_by(WorkspaceIndexViolation::Provider, None, source)
}

/// Whether a visible path receives provider syntax facts or baseline text only.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathClass {
    /// Parsed for symbols by a syntax provider.
    Source,
    /// Indexed whole (or chunked) as lexical text, with no syntax facts.
    Text,
}

/// Source and text paths [`discover`] found below one root, each list sorted by path.
#[derive(Debug, Default)]
struct DiscoveredPaths {
    source: Vec<PathBuf>,
    text: Vec<PathBuf>,
}

/// Classifies every regular file for the baseline catalog. A provider-declared
/// extension adds syntax facts without removing file content from the catalog.
#[cfg(test)]
fn classify_path(path: &Path) -> PathClass {
    if has_source_extension(path) {
        PathClass::Source
    } else {
        PathClass::Text
    }
}

/// Source and baseline text paths visible below `root`: the hard floor (`.git`, `.rift`,
/// `target`, symlinks) is always applied, `visibility.respect_gitignore()` then layers the
/// workspace's own `.gitignore` chain, and `visibility.include()`/`.exclude()` narrow or drop
/// candidate files. A provider extension adds syntax facts; every other accepted file joins
/// baseline text. Both classes use the same `.gitignore` and `[source]` policy.
///
/// Directories are walked in file-name order so a bound violation is reported
/// deterministically; both returned lists are sorted by path. Source and text paths share one
/// `files_max` budget, counted as they are discovered.
fn discover(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    language: &WorkspaceLanguagePolicy,
) -> Result<DiscoveredPaths, WorkspaceIndexError> {
    let matcher = PathMatcher::build(root, visibility.include(), visibility.exclude())?;
    let gitignore = GitignorePolicy::from_respecting(visibility.respect_gitignore());
    let mut discovered = DiscoveredPaths::default();
    for entry in source_walk(root, limits.directory_depth_max, gitignore) {
        let entry = entry.map_err(|error| walk_error(root, error))?;
        let file_type = entry.file_type();
        if file_type.is_some_and(|file_type| file_type.is_dir()) {
            if entry.depth() > limits.directory_depth_max {
                return Err(index_error_at(
                    WorkspaceIndexViolation::TooDeep,
                    entry.path(),
                ));
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
        let Some(class) = language.classifies(path)? else {
            continue;
        };
        let total = discovered.source.len() + discovered.text.len();
        if total >= limits.files_max {
            return Err(index_error_at(WorkspaceIndexViolation::TooManyFiles, path));
        }
        match class {
            ClassifiedPath::Source(_) => discovered.source.push(path.to_path_buf()),
            ClassifiedPath::Text => discovered.text.push(path.to_path_buf()),
        }
    }
    discovered.source.sort();
    discovered.text.sort();
    Ok(discovered)
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
    fn build(root: &Path, limits: WorkspaceIndexLimits) -> Result<Self, WorkspaceIndexError> {
        let mut layers = Vec::new();
        let mut ignore_files = 0_usize;
        for entry in source_walk(root, limits.directory_depth_max, GitignorePolicy::Ignore) {
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
                return Err(index_error_at(WorkspaceIndexViolation::TooManyFiles, path));
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
fn compiled_gitignore(path: &Path) -> Result<Gitignore, WorkspaceIndexError> {
    let directory = path.parent().unwrap_or(path);
    let mut builder = GitignoreBuilder::new(directory);
    if let Some(error) = builder.add(path) {
        return Err(index_error_caused_by(
            WorkspaceIndexViolation::Filesystem,
            Some(path),
            error,
        ));
    }
    builder.build().map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })
}

/// Hashes one already-discovered source and text path set without parsing syntax. Source
/// paths enforce [`WorkspaceIndexLimits::file_bytes_max`] per file, matching the bound the
/// index build applies; text paths carry no per-file bound, matching
/// [`included_text_file`] - both classes still count against the shared aggregate
/// `workspace_bytes_max`.
fn capture_paths(
    root: &Path,
    paths: &DiscoveredPaths,
    limits: WorkspaceIndexLimits,
) -> Result<WorkspaceDigests, WorkspaceIndexError> {
    let mut digests = BTreeMap::new();
    let mut workspace_bytes = 0_usize;
    capture_path_class(
        &mut digests,
        &mut workspace_bytes,
        root,
        &paths.source,
        limits,
    )?;
    capture_path_class(
        &mut digests,
        &mut workspace_bytes,
        root,
        &paths.text,
        limits,
    )?;
    Ok(WorkspaceDigests::new(digests))
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
/// Returns [`WorkspaceIndexError`] for discovery, read, or configured-bound failures.
pub fn capture_digests(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
) -> Result<WorkspaceDigests, WorkspaceIndexError> {
    capture_digests_with_languages(
        root,
        limits,
        visibility,
        &TextFileInclusion::default(),
        &LanguageFileSelections::default(),
    )
}

/// Reads one effective language and text selection's digests below `root`.
///
/// # Errors
///
/// Returns [`WorkspaceIndexError`] for configuration, discovery, read, or
/// configured-bound failures.
pub fn capture_digests_with_languages(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    text_inclusion: &TextFileInclusion,
    languages: &LanguageFileSelections,
) -> Result<WorkspaceDigests, WorkspaceIndexError> {
    let root = canonical_root(root)?;
    let language = WorkspaceLanguagePolicy::build(&root, languages, text_inclusion)?;
    let classified = discover(&root, limits, visibility, &language)?;
    capture_paths(&root, &classified, limits)
}

/// Reads one path class into captured file states.
fn capture_path_class(
    digests: &mut BTreeMap<ProjectPath, FileDigest>,
    workspace_bytes: &mut usize,
    root: &Path,
    paths: &[PathBuf],
    limits: WorkspaceIndexLimits,
) -> Result<(), WorkspaceIndexError> {
    for path in paths {
        let mut handle = fs::File::open(path).map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        let metadata = handle.metadata().map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        let mut bytes = Vec::new();
        handle.read_to_end(&mut bytes).map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        if bytes.len() > limits.file_bytes_max()
            || bytes.contains(&0)
            || std::str::from_utf8(&bytes).is_err()
        {
            continue;
        }
        *workspace_bytes = workspace_bytes
            .checked_add(bytes.len())
            .ok_or_else(|| index_error_at(WorkspaceIndexViolation::WorkspaceTooLarge, path))?;
        if *workspace_bytes > limits.workspace_bytes_max() {
            return Err(index_error_at(
                WorkspaceIndexViolation::WorkspaceTooLarge,
                path,
            ));
        }
        let project_path = project_path_below(root, path)?;
        digests.insert(
            project_path,
            FileDigest::of_file_state(&bytes, metadata_is_executable(&metadata)),
        );
    }
    Ok(())
}

impl WorkspaceDigests {
    /// The workspace identity these digests fold to.
    #[must_use]
    pub fn fingerprint(&self) -> WorkspaceFingerprint {
        WorkspaceFingerprint::from_digests(self)
    }
}

/// One file-state set over both file classes, keyed by project path.
fn keyed_digests(
    files: &BTreeMap<ProjectPath, Arc<IndexedFile>>,
    text_files: &BTreeMap<ProjectPath, Arc<TextSourceFile>>,
) -> WorkspaceDigests {
    WorkspaceDigests::new(
        files
            .iter()
            .map(|(path, file)| {
                (
                    path.clone(),
                    FileDigest::of_file_state(file.source().as_bytes(), file.executable()),
                )
            })
            .chain(text_files.iter().map(|(path, file)| {
                (
                    path.clone(),
                    FileDigest::of_file_state(file.content().as_bytes(), file.executable()),
                )
            })),
    )
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
    !is_hard_floor_name(entry.file_name())
}

/// Applies the hard floor to one absolute event path.
fn hard_floor_includes_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    !relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| WORKSPACE_IGNORED_DIRECTORIES.contains(&name))
    })
}

/// Whether `path`'s extension is one some shipped syntax provider declares
/// ([`registry::source_file_extensions`]): the walk includes exactly what a provider can
/// parse, so a new grammar joins the scan by declaring its extensions on its provider.
pub(crate) fn has_source_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| registry::source_file_extensions().contains(&extension))
}

/// Registered provider for one path claimed by provider extension.
///
/// # Panics
///
/// Panics when no provider claims the extension: a path reaches this lookup
/// only after [`has_source_extension`] proved some provider claims it, so a
/// miss is a programmer error in the gate, not a reachable input.
pub(crate) fn syntax_provider_for(path: &Path) -> &'static dyn SyntaxProvider {
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or_default();
    registry::provider_for_extension(extension).unwrap_or_else(|| {
        unreachable!(
            "an included source path must name a provider-claimed extension: path={}",
            path.display()
        )
    })
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

fn walk_error(root: &Path, error: ignore::Error) -> WorkspaceIndexError {
    let path = walk_source_path(&error).unwrap_or_else(|| root.to_path_buf());
    index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(&path), error)
}

fn read_file(
    root: &Path,
    path: &Path,
    provider: &dyn SyntaxProvider,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<IndexedFile, WorkspaceIndexError> {
    let handle = fs::File::open(path).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let metadata = handle.metadata().map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let mut bytes = Vec::new();
    handle
        .take(limits.file_bytes_max().saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
    let project_path = project_path_below(root, path)?;
    let mut file = included_file(project_path, bytes, path, provider, limits, workspace_bytes)?;
    file.executable = metadata_is_executable(&metadata);
    Ok(file)
}

pub(crate) fn indexed_file_from_catalog(
    file: &TextSourceFile,
    context_path: &Path,
    provider: &dyn SyntaxProvider,
) -> Result<IndexedFile, WorkspaceIndexError> {
    let syntax = provider
        .analyze(SyntaxSource {
            path: file.path(),
            text: file.content(),
        })
        .map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Syntax, Some(context_path), error)
        })?;
    Ok(IndexedFile {
        path: file.path().clone(),
        source: file.content().to_owned(),
        digest: file.digest(),
        executable: file.executable(),
        syntax,
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
) -> Result<IndexedFile, WorkspaceIndexError> {
    if bytes.len() > limits.file_bytes_max {
        return Err(index_error_at(
            WorkspaceIndexViolation::FileTooLarge,
            context_path,
        ));
    }
    *workspace_bytes = workspace_bytes
        .checked_add(bytes.len())
        .ok_or_else(|| index_error_at(WorkspaceIndexViolation::WorkspaceTooLarge, context_path))?;
    if *workspace_bytes > limits.workspace_bytes_max {
        return Err(index_error_at(
            WorkspaceIndexViolation::WorkspaceTooLarge,
            context_path,
        ));
    }
    let source = source_utf8(bytes, context_path)?;
    let syntax = provider
        .analyze(SyntaxSource {
            path: &project_path,
            text: &source,
        })
        .map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Syntax, Some(context_path), error)
        })?;
    Ok(IndexedFile {
        path: project_path,
        digest: FileDigest::of(source.as_bytes()),
        source,
        executable: false,
        syntax,
    })
}

#[cfg(test)]
fn read_text_file(
    root: &Path,
    path: &Path,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<TextSourceFile, WorkspaceIndexError> {
    let bytes = fs::read(path).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let metadata = fs::metadata(path).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let project_path = project_path_below(root, path)?;
    let mut file = included_text_file(project_path, bytes, path, limits, workspace_bytes)?;
    file.executable = metadata_is_executable(&metadata);
    Ok(file)
}

/// Reads one baseline catalog candidate with bounded binary detection.
fn read_catalog_file(
    root: &Path,
    path: &Path,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<CatalogRead, WorkspaceIndexError> {
    let handle = fs::File::open(path).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let metadata = handle.metadata().map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let mut bytes = Vec::new();
    handle
        .take(limits.file_bytes_max().saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
    let project_path = project_path_below(root, path)?;
    if bytes.len() > limits.file_bytes_max() {
        return Ok(CatalogRead::Skipped(WorkspaceIndexWarning::FileTooLarge(
            project_path,
        )));
    }
    if bytes.contains(&0) {
        return Ok(CatalogRead::Skipped(WorkspaceIndexWarning::BinarySource(
            project_path,
        )));
    }
    if std::str::from_utf8(&bytes).is_err() {
        return Ok(CatalogRead::Skipped(
            WorkspaceIndexWarning::InvalidUtf8Source(project_path),
        ));
    }
    let mut file = included_text_file(project_path, bytes, path, limits, workspace_bytes)?;
    file.executable = metadata_is_executable(&metadata);
    Ok(CatalogRead::Included(file))
}

#[cfg(unix)]
fn metadata_is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn metadata_is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

/// Includes one UTF-8 file in the baseline content catalog.
pub(crate) fn included_text_file(
    project_path: ProjectPath,
    bytes: Vec<u8>,
    context_path: &Path,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<TextSourceFile, WorkspaceIndexError> {
    *workspace_bytes = workspace_bytes
        .checked_add(bytes.len())
        .ok_or_else(|| index_error_at(WorkspaceIndexViolation::WorkspaceTooLarge, context_path))?;
    if *workspace_bytes > limits.workspace_bytes_max {
        return Err(index_error_at(
            WorkspaceIndexViolation::WorkspaceTooLarge,
            context_path,
        ));
    }
    let content = source_utf8(bytes, context_path)?;
    Ok(TextSourceFile {
        path: project_path,
        digest: FileDigest::of(content.as_bytes()),
        content,
        executable: false,
    })
}

/// Decides whether bytes read for one claimed file are valid UTF-8 source: the single
/// classification source discovery's request-time capture, index construction, and every
/// direct file read share. Invalid bytes refuse; the caller decides whether that refusal
/// fails its own operation outright (a single-file read) or is instead treated as an
/// omission (a whole-workspace build or capture).
fn source_utf8(bytes: Vec<u8>, context_path: &Path) -> Result<String, WorkspaceIndexError> {
    String::from_utf8(bytes).map_err(|error| {
        index_error_caused_by(
            WorkspaceIndexViolation::InvalidSource,
            Some(context_path),
            error,
        )
    })
}

/// The project-relative address of `absolute`, which the caller has already proven lies
/// below `root`. Every direct filesystem read into the index shares this conversion, so a
/// discovered path and a path recovered after a skipped read resolve to the same
/// [`ProjectPath`].
fn project_path_below(root: &Path, absolute: &Path) -> Result<ProjectPath, WorkspaceIndexError> {
    let relative = absolute.strip_prefix(root).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::InvalidPath, Some(absolute), error)
    })?;
    relative_path(relative)
}

fn relative_path(path: &Path) -> Result<ProjectPath, WorkspaceIndexError> {
    let value = path
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| index_error_at(WorkspaceIndexViolation::InvalidPath, path))?
        .join("/");
    ProjectPath::new(value).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::InvalidPath, Some(path), error)
    })
}

fn symbol_rank(symbol: &SyntaxSymbol, query: &str) -> SymbolMatchRank {
    let qualified = symbol.qualified_name.to_lowercase();
    let name = symbol.name.to_lowercase();
    if qualified == query {
        SymbolMatchRank::QualifiedExact
    } else if name == query {
        SymbolMatchRank::NameExact
    } else if name.starts_with(query) {
        SymbolMatchRank::NamePrefix
    } else {
        SymbolMatchRank::Substring
    }
}

/// The declaration's exact source text, clamped to `file`'s bounds the same way the read
/// service excerpts a symbol's source.
fn declaration_source(file: &IndexedFile, range: rift_syntax::ByteRange) -> &str {
    let source = file.source();
    let start = usize::try_from(range.start)
        .unwrap_or(source.len())
        .min(source.len());
    let end = usize::try_from(range.end)
        .unwrap_or(source.len())
        .min(source.len());
    source.get(start..end).unwrap_or_default()
}

/// One lexical unit for a symbol declaration. `identity` is minted by the same
/// [`rift_core::symbol_identity`] the read service uses for that declaration's wire
/// `SymbolId`, so a lexical hit's identity equals the id `get_symbol` returns for it.
fn symbol_lexical_unit(file: &IndexedFile, symbol: &SyntaxSymbol) -> LexicalUnit {
    let identity = rift_core::symbol_identity(
        &file.syntax().language().identity_segment(),
        file.path().as_str(),
        &symbol.qualified_name,
    );
    let content = declaration_source(file, symbol.range).to_owned();
    LexicalUnit::new(
        identity,
        file.path().clone(),
        LexicalUnitKind::Symbol,
        Some(symbol.name.clone()),
        content,
    )
    .unwrap_or_else(|error| {
        unreachable!("a symbol's rift identity must be non-empty: error={error}")
    })
}

/// The file stem of `path`'s final segment, when it has one.
fn file_stem(path: &ProjectPath) -> Option<String> {
    Path::new(path.as_str())
        .file_stem()
        .and_then(OsStr::to_str)
        .map(str::to_owned)
}

/// Whether `content_bytes` bytes of text-file content exceed `chunk_bytes_max`, the bound
/// past which [`WorkspaceIndex::lexical_units`] chunks the file instead of publishing it
/// whole.
fn exceeds_chunk_bound(content_bytes: usize, chunk_bytes_max: u64) -> bool {
    u64::try_from(content_bytes).unwrap_or(u64::MAX) > chunk_bytes_max
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

/// Appends one text file's lexical units to `units`: one whole unit within `chunk_bytes_max`,
/// one unit per chunk otherwise, every chunk sharing the file's real path and file-stem name
/// so a hit still maps back to the file it came from.
fn push_text_lexical_units(
    units: &mut Vec<LexicalUnit>,
    file: &TextSourceFile,
    chunk_bytes_max: u64,
) {
    let name = file_stem(file.path());
    if !exceeds_chunk_bound(file.content().len(), chunk_bytes_max) {
        units.push(new_text_lexical_unit(
            file.path().as_str().to_owned(),
            file,
            name,
            file.content().to_owned(),
        ));
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
        units.push(new_text_lexical_unit(
            identity,
            file,
            name.clone(),
            chunk.content().to_owned(),
        ));
    }
}

/// Constructs one text-file lexical unit, naming the failure mode this crate guarantees
/// never fires: every identity built here is a non-empty path or path-plus-chunk-index.
fn new_text_lexical_unit(
    identity: String,
    file: &TextSourceFile,
    name: Option<String>,
    content: String,
) -> LexicalUnit {
    LexicalUnit::new(
        identity,
        file.path().clone(),
        LexicalUnitKind::TextFile,
        name,
        content,
    )
    .unwrap_or_else(|error| {
        unreachable!("a text file's lexical identity must be non-empty: error={error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_syntax::{RustSyntaxProvider, SyntaxLimits};
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
        WorkspaceIndex::indexed_bytes(&index.files, &index.text_files)
    }

    /// Resolves the paths named against `index`, reading each one's current bytes.
    fn resolved(index: &WorkspaceIndex, root: &Path, names: &[&str]) -> PathChanges {
        let observed = names.iter().map(|name| {
            let path = ProjectPath::new(*name).expect("fixture path must be valid");
            let digest = fs::read(root.join(name))
                .ok()
                .map(|bytes| FileDigest::of(&bytes));
            (path, digest)
        });
        PathChanges::resolve(observed, |path| index.digest(path))
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
            policy.includes(&canonical.join("src/lib.rs")),
            "the policy keeps the same file the walk kept"
        );
        assert!(
            index
                .file(&ProjectPath::new(".ruff_cache/cached.rs").expect("fixture path"))
                .is_none(),
            "the walk drops what the nested ignore file names"
        );
        assert!(
            !policy.includes(&canonical.join(".ruff_cache/cached.rs")),
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
        let inclusion = TextFileInclusion::new(1_024);

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
    fn test_rebuilt_reindexes_an_edited_text_file() {
        let directory = fixture();
        let root = directory.path();
        let text_inclusion = TextFileInclusion::new(1_024);
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
        let text_inclusion = TextFileInclusion::new(1_024);
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
            error.fault().violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge
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

    #[test]
    fn test_rebuilt_omits_a_newly_invalid_file_and_recovers_a_fixed_one() {
        let directory = fixture();
        let root = directory.path();
        let index = indexed(root, &TextFileInclusion::default());
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
        assert_eq!(
            corrupted.warnings(),
            [WorkspaceIndexWarning::InvalidUtf8Source(lib_path.clone())],
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
        assert_eq!(
            oversized.warnings(),
            [WorkspaceIndexWarning::FileTooLarge(lib_path.clone())]
        );

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
            !policy.includes(&canonical.join("nested/hidden.rs")),
            "the nested file excludes the path it names"
        );
        assert!(
            policy.includes(&canonical.join("hidden.rs")),
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
            policy.includes(&canonical.join("nested/kept.rs")),
            "the walk and the policy must agree on a re-included path"
        );
        assert_eq!(
            index
                .file(&ProjectPath::new("dropped.rs").expect("fixture path"))
                .is_some(),
            policy.includes(&canonical.join("dropped.rs")),
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
        assert!(!index.nodes(&path, 4).expect("indexed path").is_empty());
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
        assert!(policy.includes(&directory.path().join("src/lib.rs")));
        assert!(!policy.includes(&directory.path().join("src/ignored.rs")));
        assert!(!policy.includes(&directory.path().join("src/generated/code.rs")));
        assert!(!policy.includes(&directory.path().join("target/code.rs")));
        assert!(
            policy.visible(&directory.path().join("src/logo.png")),
            "visibility does not depend on a file extension"
        );
        assert!(!policy.includes(&directory.path().join("src/logo.png")));
        assert!(!policy.includes(Path::new("outside.rs")));
        assert!(policy.may_include_descendant(&directory.path().join("src")));
        assert!(!policy.may_include_descendant(&directory.path().join("examples")));
        assert!(!policy.may_include_descendant(&directory.path().join("src/generated")));
        assert!(!policy.may_include_descendant(&directory.path().join("target")));
        let canonical_root = fs::canonicalize(directory.path()).expect("canonical workspace");
        assert!(policy.includes(&canonical_root.join("src/lib.rs")));
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

        assert!(policy.includes(&directory.path().join("docs/guide.mdx")));
        assert!(policy.includes(&directory.path().join("notes.txt")));
        assert!(
            !policy.includes(&directory.path().join("docs/ignored.mdx")),
            "gitignore must hide a text candidate exactly as it would a source one"
        );
        assert!(
            !policy.includes(&directory.path().join("docs/generated/gen.mdx")),
            "[source] exclude must hide a text candidate exactly as it would a source one"
        );
        assert!(
            policy.visible(&directory.path().join("logo.png")),
            "visibility does not depend on a file extension"
        );
        assert!(!policy.includes(&directory.path().join("logo.png")));
    }

    #[test]
    fn test_workspace_source_policy_visible_reaches_files_no_syntax_provider_parses() {
        let directory = fixture();
        fs::create_dir(directory.path().join("target")).expect("target directory");
        fs::write(directory.path().join(".gitignore"), "hidden.mdx\n").expect("ignore policy");
        fs::write(directory.path().join("justfile"), "default:\n    echo hi\n")
            .expect("no extension at all");
        fs::write(directory.path().join("notes.ini"), "[section]\nkey = 1\n")
            .expect("an extension no provider or text policy claims");
        fs::write(directory.path().join("guide.mdx"), "guide").expect("no syntax provider");
        fs::write(directory.path().join("hidden.mdx"), "secret").expect("gitignored candidate");
        fs::write(
            directory.path().join("excluded.rs"),
            "pub fn excluded() {}\n",
        )
        .expect("excluded candidate");
        let visibility = SourceVisibility::new(Vec::new(), vec!["excluded.rs".to_owned()], true);
        let policy = WorkspaceSourcePolicy::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
        )
        .expect("source policy");

        assert!(
            policy.visible(&directory.path().join("justfile")),
            "a tracked extensionless file is visible - patch reaches it"
        );
        assert!(
            !policy.includes(&directory.path().join("justfile")),
            "an extensionless file needs an explicit text pattern"
        );
        assert!(
            policy.visible(&directory.path().join("notes.ini")),
            "a tracked file no provider or text policy claims is still visible"
        );
        assert!(
            !policy.includes(&directory.path().join("notes.ini")),
            "a visible file joins lexical search only through the text pattern"
        );
        assert!(
            policy.visible(&directory.path().join("guide.mdx")),
            "a tracked file no syntax provider parses is visible"
        );
        assert!(policy.includes(&directory.path().join("guide.mdx")));
        assert!(
            !policy.visible(&directory.path().join("target/build.log")),
            "the hard floor refuses visibility below it, the same as inclusion"
        );
        assert!(
            !policy.visible(&directory.path().join("excluded.rs")),
            "a [source].exclude match refuses visibility, not only inclusion"
        );
        assert!(
            !policy.visible(&directory.path().join("hidden.mdx")),
            "a gitignored path refuses visibility, not only inclusion"
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
    ) -> Result<WorkspaceIndex, WorkspaceIndexError> {
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
            bound_error.fault().violation(),
            WorkspaceIndexViolation::TooManyFiles
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
        assert_eq!(error.fault().violation(), WorkspaceIndexViolation::TooDeep);
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::SourcePatternInvalid
        );
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::SourcePatternInvalid
        );
    }

    #[test]
    fn test_invalid_exclude_glob_reports_source_pattern_invalid() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("lib.rs"), "pub fn kept() {}\n").expect("kept source");

        let visibility = SourceVisibility::new(Vec::new(), vec!["[".to_owned()], true);
        let error = build_index(&directory, &visibility).expect_err("unclosed glob class");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::SourcePatternInvalid
        );
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
                .all(|warning| matches!(warning, WorkspaceIndexWarning::FileTooLarge(_)))
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
                .fault()
                .violation(),
            WorkspaceIndexViolation::ResultLimit,
        );
        assert_eq!(
            index
                .source_matches("Rift", 0)
                .expect_err("zero result bound")
                .fault()
                .violation(),
            WorkspaceIndexViolation::ResultLimit,
        );
    }

    #[test]
    fn test_index_enforces_scan_bounds_and_root_contract() {
        assert_eq!(
            WorkspaceIndexLimits::new(0, 1, 1, 1, 1)
                .expect_err("zero bound")
                .fault()
                .violation(),
            WorkspaceIndexViolation::ZeroLimit,
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
            missing_error.fault().violation(),
            WorkspaceIndexViolation::InvalidRoot
        );
        assert_eq!(missing_error.fault().path(), Some(missing.as_path()));
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
            .fault()
            .violation(),
            WorkspaceIndexViolation::InvalidRoot,
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
            .fault()
            .violation(),
            WorkspaceIndexViolation::TooManyFiles,
        );
        assert_eq!(
            WorkspaceIndex::build(
                directory.path(),
                WorkspaceIndexLimits::new(5, 1_000, 8, 4, 5).expect("limits"),
                &SourceVisibility::default(),
                &rift_core::TextFileInclusion::default(),
            )
            .expect_err("workspace byte bound")
            .fault()
            .violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge,
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
            .fault()
            .violation(),
            WorkspaceIndexViolation::TooDeep,
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
        assert_eq!(exact[0].rank, SymbolMatchRank::QualifiedExact);
        assert_eq!(
            index.symbols("update", 5).expect("name match")[0].rank,
            SymbolMatchRank::NameExact
        );
        assert_eq!(
            index.symbols("upd", 5).expect("prefix match")[0].rank,
            SymbolMatchRank::NamePrefix
        );
        assert_eq!(
            index.symbols("pda", 5).expect("substring match")[0].rank,
            SymbolMatchRank::Substring
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
        assert!(index.nodes(&missing, 0).is_none());
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
        assert_eq!(error.fault().violation(), WorkspaceIndexViolation::Provider);
        let source =
            std::error::Error::source(&error).expect("provider failure must retain missing record");
        assert!(
            source.to_string().contains("foreign"),
            "missing record must name identity: {source}"
        );
    }

    #[test]
    fn test_index_error_messages_are_stable() {
        let cases = [
            (
                WorkspaceIndexViolation::ZeroLimit,
                "the workspace configuration failed validation: violation zero_limit; \
                 correct the reported configuration field, then retry",
            ),
            (
                WorkspaceIndexViolation::InvalidRoot,
                "the workspace configuration failed validation: violation invalid_root; \
                 correct the reported configuration field, then retry",
            ),
            (
                WorkspaceIndexViolation::TooDeep,
                "the request exceeded a declared resource limit: violation too_deep; \
                 resize the request below the named limit, or raise that limit \
                 in the workspace configuration",
            ),
            (
                WorkspaceIndexViolation::TooManyFiles,
                "the request exceeded a declared resource limit: violation too_many_files; \
                 resize the request below the named limit, or raise that limit \
                 in the workspace configuration",
            ),
            (
                WorkspaceIndexViolation::FileTooLarge,
                "the request exceeded a declared resource limit: violation file_too_large; \
                 resize the request below the named limit, or raise that limit \
                 in the workspace configuration",
            ),
            (
                WorkspaceIndexViolation::WorkspaceTooLarge,
                "the request exceeded a declared resource limit: violation workspace_too_large; \
                 resize the request below the named limit, or raise that limit \
                 in the workspace configuration",
            ),
            (
                WorkspaceIndexViolation::InvalidPath,
                "the path cannot be addressed by this workspace: violation invalid_path; \
                 use a workspace-relative path with `/` separators and no `.` or `..` components",
            ),
            (
                WorkspaceIndexViolation::InvalidSource,
                "the addressed content exists but its bytes cannot be served: \
                 violation invalid_source; request the declaration without its body, \
                 or read a source-backed unit",
            ),
            (
                WorkspaceIndexViolation::Filesystem,
                "workspace state could not be read or written: violation filesystem; \
                 check filesystem permissions and free space, then retry",
            ),
            (
                WorkspaceIndexViolation::Syntax,
                "the server failed in a way it did not classify: violation syntax; \
                 retry once, and report the full message if the failure repeats",
            ),
            (
                WorkspaceIndexViolation::Composition,
                "the workspace configuration failed validation: violation composition; \
                 correct the reported configuration field, then retry",
            ),
            (
                WorkspaceIndexViolation::ResultLimit,
                "the request exceeded a declared resource limit: violation result_limit; \
                 resize the request below the named limit, or raise that limit \
                 in the workspace configuration",
            ),
            (
                WorkspaceIndexViolation::SourcePatternInvalid,
                "the workspace configuration failed validation: violation source_pattern_invalid; \
                 correct the reported configuration field, then retry",
            ),
            (
                WorkspaceIndexViolation::History,
                "the server failed in a way it did not classify: violation history; \
                 retry once, and report the full message if the failure repeats",
            ),
        ];
        for (violation, message) in cases {
            assert_eq!(index_error(violation).to_string(), message);
        }
    }

    #[test]
    fn test_error_display_appends_offending_path() {
        let error = index_error_at(
            WorkspaceIndexViolation::FileTooLarge,
            Path::new("src/big.rs"),
        );
        assert_eq!(
            error.to_string(),
            "the request exceeded a declared resource limit: \
             violation file_too_large, path src/big.rs; \
             resize the request below the named limit, or raise that limit \
             in the workspace configuration"
        );
    }

    #[test]
    fn test_component_identity_failure_surfaces_as_composition_error() {
        let error = component::<(), WorkspaceFiles>("").expect_err("empty component id");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::Composition
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn test_read_file_classifies_syntax_and_non_nfc_path_failures() {
        let directory = fixture();
        let limits = WorkspaceIndexLimits::default();
        let mut bytes = 0;
        let strict_parser =
            RustSyntaxProvider::new(SyntaxLimits::new(1, 1, 1).expect("positive bounds"));
        let source_path = directory.path().join("src/lib.rs");
        let syntax_error = read_file(
            directory.path(),
            &source_path,
            &strict_parser,
            limits,
            &mut bytes,
        )
        .expect_err("syntax byte bound");
        assert_eq!(
            syntax_error.fault().violation(),
            WorkspaceIndexViolation::Syntax
        );
        assert_eq!(syntax_error.fault().path(), Some(source_path.as_path()));
        assert!(std::error::Error::source(&syntax_error).is_some());
        assert_eq!(
            syntax_error.descriptor().code(),
            "limit_exceeded",
            "a syntax failure must keep the underlying syntax classification"
        );

        let parser = RustSyntaxProvider::default();
        let decomposed = directory.path().join("src/cafe\u{301}.rs");
        fs::write(&decomposed, "fn accent() {}").expect("decomposed source");
        let path_error = read_file(directory.path(), &decomposed, &parser, limits, &mut bytes)
            .expect_err("non-NFC project path");
        assert_eq!(
            path_error.fault().violation(),
            WorkspaceIndexViolation::InvalidPath
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
        assert_eq!(
            unreadable.fault().violation(),
            WorkspaceIndexViolation::Filesystem
        );
        assert_eq!(unreadable.fault().path(), Some(locked.as_path()));

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
        assert_eq!(
            stat_error.fault().violation(),
            WorkspaceIndexViolation::Filesystem
        );
        assert_eq!(
            stat_error.fault().path(),
            Some(unsearchable.join("entry.rs").as_path())
        );
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
                .fault()
                .violation(),
            WorkspaceIndexViolation::Filesystem,
        );

        let outside = tempfile::tempdir().expect("outside directory");
        let outside_file = outside.path().join("outside.rs");
        fs::write(&outside_file, "fn outside() {}").expect("outside source");
        assert_eq!(
            read_file(directory.path(), &outside_file, &parser, limits, &mut bytes,)
                .expect_err("outside project path")
                .fault()
                .violation(),
            WorkspaceIndexViolation::InvalidPath,
        );

        let invalid = directory.path().join("src/invalid.rs");
        fs::write(&invalid, [0xff]).expect("invalid UTF-8");
        assert_eq!(
            read_file(directory.path(), &invalid, &parser, limits, &mut bytes)
                .expect_err("the file's own read refuses rather than returning empty content")
                .fault()
                .violation(),
            WorkspaceIndexViolation::InvalidSource,
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
            .fault()
            .violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge,
        );
    }

    /// Builds a [`DiscoveredPaths`] with `source` classified as source and no text paths, for
    /// tests exercising [`capture_paths`] directly.
    fn source_only(source: Vec<PathBuf>) -> DiscoveredPaths {
        DiscoveredPaths {
            source,
            text: Vec::new(),
        }
    }

    #[test]
    fn test_capture_paths_preserves_bound_and_path_failures() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let limits = WorkspaceIndexLimits::new(5, 8, 10, 4, 5).expect("limits");

        let missing = root.join("missing.rs");
        let error =
            capture_paths(&root, &source_only(vec![missing]), limits).expect_err("missing source");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::Filesystem
        );

        let oversized = root.join("oversized.rs");
        fs::write(&oversized, b"123456789").expect("oversized source");
        let digests = capture_paths(&root, &source_only(vec![oversized]), limits)
            .expect("oversized source is omitted");
        assert!(digests.is_empty());

        let first = root.join("first.rs");
        let second = root.join("second.rs");
        fs::write(&first, b"123456").expect("first source");
        fs::write(&second, b"123456").expect("second source");
        let error = capture_paths(&root, &source_only(vec![first, second]), limits)
            .expect_err("workspace bound");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge
        );

        let outside = tempfile::NamedTempFile::new().expect("outside source");
        fs::write(outside.path(), b"fn x(){}").expect("outside bytes");
        let error = capture_paths(
            &root,
            &source_only(vec![outside.path().to_path_buf()]),
            limits,
        )
        .expect_err("outside path");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidPath
        );

        // Unlike every other case above, invalid UTF-8 does not fail the capture: the file
        // is omitted from the digest set instead, matching what a build omits from the
        // index over the same tree.
        let invalid = root.join("invalid.rs");
        fs::write(&invalid, [0xff]).expect("invalid source");
        let digests = capture_paths(&root, &source_only(vec![invalid]), limits)
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
        };
        let digests = capture_paths(&root, &paths, limits)
            .expect("a text file over file_bytes_max is omitted");
        assert!(digests.is_empty());

        let tight = WorkspaceIndexLimits::new(5, 4, 10, 4, 5).expect("limits");
        let over_workspace = root.join("over.md");
        fs::write(&over_workspace, b"still more than ten bytes total").expect("oversized text");
        let paths = DiscoveredPaths {
            source: Vec::new(),
            text: vec![over_workspace],
        };
        let digests =
            capture_paths(&root, &paths, tight).expect("the per-file bound applies first");
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
        };
        let digests = capture_paths(&root, &paths, limits)
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::TooManyFiles
        );
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::Filesystem
        );
    }

    #[test]
    fn test_classify_path_provider_extension_selects_source() {
        assert_eq!(classify_path(Path::new("lib.rs")), PathClass::Source);
    }

    #[test]
    fn test_classify_path_returns_text_for_a_non_source_extension() {
        assert_eq!(classify_path(Path::new("readme.txt")), PathClass::Text);
    }

    #[test]
    fn test_classify_path_provider_claims_markdown() {
        assert_eq!(classify_path(Path::new("readme.md")), PathClass::Source);
    }

    #[test]
    fn test_classify_path_provider_claims_json_and_yaml() {
        for path in ["config.json", "deploy.yaml", "deploy.yml"] {
            assert_eq!(
                classify_path(Path::new(path)),
                PathClass::Source,
                "provider must claim {path}"
            );
        }
    }

    #[test]
    fn test_classify_path_returns_text_for_unknown_extension() {
        assert_eq!(classify_path(Path::new("notes.unknown")), PathClass::Text);
    }

    /// `justfile` carries no extension. It is visible by name and joins baseline text.
    #[test]
    fn test_classify_path_returns_text_for_an_extensionless_file_with_no_nul() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let path = directory.path().join("justfile");
        fs::write(&path, "build:\n\tcargo build\n").expect("justfile");
        assert_eq!(classify_path(&path), PathClass::Text);
    }

    #[test]
    fn test_classify_path_defers_binary_detection_until_bounded_read() {
        assert_eq!(classify_path(Path::new("artifact")), PathClass::Text);
    }

    /// A provider claims a path by extension alone, so an extensionless file never becomes
    /// `Source`, whatever a syntax provider could parse from its content: the default
    /// binary detection applies later during bounded catalog read.
    #[test]
    fn test_classify_path_extensionless_file_never_classifies_as_source() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let path = directory.path().join("Dockerfile");
        fs::write(&path, "FROM scratch\n").expect("dockerfile");
        assert_ne!(classify_path(&path), PathClass::Source);
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
        assert_eq!(text_paths, ["docs/guide.md", "docs/notes.mdx"]);
        let source_paths: Vec<&str> = index.files().map(|file| file.path().as_str()).collect();
        assert_eq!(source_paths, ["docs/guide.md"]);
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
        let units = index.lexical_units();
        assert_eq!(
            units
                .iter()
                .filter(|unit| unit.kind() == LexicalUnitKind::TextFile)
                .count(),
            1,
            "provider content must produce one whole-file unit: {units:#?}"
        );
        assert_eq!(
            units
                .iter()
                .filter(|unit| unit.kind() == LexicalUnitKind::Symbol)
                .count(),
            1,
            "the heading remains a syntax fact: {units:#?}"
        );
        assert!(
            units.iter().all(|unit| unit.path().as_str() == "README.md"),
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
        let units = index.lexical_units();
        let identities: Vec<&str> = units.iter().map(LexicalUnit::identity).collect();
        assert!(identities.contains(&"rift://symbol/json/config.json/server"));
        assert!(identities.contains(&"rift://symbol/yaml/deploy.yml/retries"));
        assert_eq!(
            units
                .iter()
                .filter(|unit| unit.kind() == LexicalUnitKind::TextFile)
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
        assert_eq!(
            index.warnings(),
            [WorkspaceIndexWarning::InvalidUtf8Source(invalid_path)],
            "the build carries a warning naming the skipped file"
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
        assert_eq!(
            index.warnings(),
            &[
                WorkspaceIndexWarning::BinarySource(ProjectPath::new("binary.rs").expect("path")),
                WorkspaceIndexWarning::FileTooLarge(ProjectPath::new("large.rs").expect("path")),
            ]
        );
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
                .map(|(path, digest)| (path.clone(), Some(digest))),
            |path| index.digests().get(path),
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
        assert_eq!(
            index.warnings(),
            [WorkspaceIndexWarning::InvalidUtf8Source(invalid_path)],
            "the build carries a warning naming the skipped file"
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::TooManyFiles
        );
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
        let units = index.lexical_units();
        let update = units
            .iter()
            .find(|unit| unit.kind() == LexicalUnitKind::Symbol && unit.name() == Some("update"))
            .expect("the update symbol must produce a lexical unit");
        assert_eq!(
            update.identity(),
            "rift://symbol/rust/src/lib.rs/Rift::update"
        );
        assert_eq!(update.path().as_str(), "src/lib.rs");
        assert_eq!(update.content(), "pub fn update() {}");
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
        let units = index.lexical_units();
        let text_unit = units
            .iter()
            .find(|unit| unit.kind() == LexicalUnitKind::TextFile)
            .expect("the text file must produce a lexical unit");
        assert_eq!(text_unit.identity(), "guide.txt");
        assert_eq!(text_unit.name(), Some("guide"));
        assert_eq!(text_unit.content(), "guide body");
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
        let inclusion = TextFileInclusion::new(10);
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &inclusion,
        )
        .expect("workspace index");

        let units: Vec<_> = index
            .lexical_units()
            .into_iter()
            .filter(|unit| unit.kind() == LexicalUnitKind::TextFile)
            .collect();
        assert_eq!(
            units.len(),
            4,
            "an 8-line file chunked two lines at a time yields 4 units"
        );
        let identities: Vec<&str> = units.iter().map(LexicalUnit::identity).collect();
        assert_eq!(
            identities,
            ["big.txt#0", "big.txt#1", "big.txt#2", "big.txt#3"]
        );
        let rejoined: String = units.iter().map(LexicalUnit::content).collect();
        assert_eq!(
            rejoined, content,
            "chunk content must reconstruct the file exactly"
        );
        for unit in &units {
            assert_eq!(unit.name(), Some("big"));
            assert_eq!(unit.path().as_str(), "big.txt");
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::Filesystem
        );
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidPath
        );
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
            error.fault().violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidSource
        );
    }

    #[test]
    #[should_panic(expected = "a text file's lexical identity must be non-empty")]
    fn test_push_text_lexical_units_of_root_path_panics_on_empty_identity() {
        // `ProjectPath::new("")` is valid and names the workspace root, so a whole-file text
        // unit for it builds its identity from that empty path, which `LexicalUnit::new`
        // refuses: this invariant must never fire for a real discovered path.
        let file = TextSourceFile {
            path: ProjectPath::new("").expect("an empty project path names the workspace root"),
            digest: FileDigest::of(b"hello"),
            content: "hello".to_owned(),
            executable: false,
        };
        let mut units = Vec::new();
        push_text_lexical_units(&mut units, &file, 1_024);
    }
}
