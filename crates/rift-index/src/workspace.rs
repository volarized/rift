use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, Match, Walk, WalkBuilder};
pub use rift_analysis::IndexedFile;
use rift_analysis::documentation::{
    DocumentationCollection, DocumentationError, DocumentationLayer,
};
use rift_core::constants::{
    READ_RESULTS_MAX_DEFAULT, VCS_IGNORE_FILE, WORKSPACE_BYTES_MAX_DEFAULT,
    WORKSPACE_CONFIGURATION_FILE, WORKSPACE_DECLARATIONS_MAX_DEFAULT,
    WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT, WORKSPACE_FILES_MAX_DEFAULT,
    WORKSPACE_IGNORED_DIRECTORIES,
};
use rift_core::{
    CompositionId, ContributionError, Error, ErrorCode, ErrorContext, ErrorName, Fault,
    LanguageFileSelections, LimitEvidence, PortableSymbolFacts, ProjectPath, ProviderId,
    SourceVisibility, SymbolId, TextFileInclusion, fault_label, symbol_identity,
};
use rift_protocol::configuration::SyntaxConfiguration;
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
use rift_syntax::{
    SyntaxError, SyntaxLimits, SyntaxNode, SyntaxProvider, SyntaxSource, SyntaxSymbol,
    SyntaxViolation, registry,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::change_set::{FileDigest, PathChanges, WorkspaceDigests, tree_revision_of};
use crate::chunk::text_chunks;
use crate::documentation::NotebookFiles;
use crate::glob::{ForceIncludeReach, PathMatcher, PathVerdict};
use crate::language::{ClassifiedPath, LanguagePolicyError, WorkspaceLanguagePolicy};
use crate::lexical::LimitBreach;
use crate::relationship::RelationshipStore;
use crate::semantic::{BuiltSemantics, WorkspaceSemanticError, WorkspaceSemantics};

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
    /// Returns [`WorkspaceIndexError`] when any bound is zero.
    pub fn new(
        files_max: usize,
        file_bytes_max: usize,
        workspace_bytes_max: usize,
        directory_depth_max: usize,
        results_max: usize,
    ) -> Result<Self, WorkspaceIndexError> {
        Self {
            files_max,
            file_bytes_max,
            workspace_bytes_max,
            declarations_max: WORKSPACE_DECLARATIONS_MAX_DEFAULT,
            directory_depth_max,
            results_max,
            syntax: SyntaxLimits::default(),
        }
        .validated()
    }

    /// Refuses any bound that is zero, so no build runs under a bound it cannot meet.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when any bound is zero.
    fn validated(self) -> Result<Self, WorkspaceIndexError> {
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
    /// Returns [`WorkspaceIndexError`] when any of the three bounds is zero.
    pub fn with_workspace_bounds(
        self,
        files_max: usize,
        workspace_bytes_max: usize,
        declarations_max: usize,
    ) -> Result<Self, WorkspaceIndexError> {
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

    /// Parses every source under `syntax`; the per-file byte bound follows its source bound,
    /// so the walk admits every file a provider accepts.
    #[must_use]
    pub const fn with_syntax(self, syntax: SyntaxLimits) -> Self {
        Self {
            file_bytes_max: syntax.source_bytes_max(),
            syntax,
            ..self
        }
    }

    /// Parses every source under a `[providers.syntax]` table's bounds, as
    /// [`Self::with_syntax`] does.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when the table states a zero bound.
    pub fn with_syntax_configuration(
        self,
        configuration: &SyntaxConfiguration,
    ) -> Result<Self, WorkspaceIndexError> {
        let syntax = SyntaxLimits::from_configuration(configuration).map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::ZeroLimit, None, error)
        })?;
        Ok(self.with_syntax(syntax))
    }

    /// Syntax bounds every source parses under.
    #[must_use]
    pub const fn syntax(self) -> SyntaxLimits {
        self.syntax
    }
}

impl Default for WorkspaceIndexLimits {
    fn default() -> Self {
        Self {
            files_max: WORKSPACE_FILES_MAX_DEFAULT,
            file_bytes_max: registry::file_bytes_max_default(),
            workspace_bytes_max: WORKSPACE_BYTES_MAX_DEFAULT,
            declarations_max: WORKSPACE_DECLARATIONS_MAX_DEFAULT,
            directory_depth_max: WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT,
            results_max: READ_RESULTS_MAX_DEFAULT,
            syntax: SyntaxLimits::default(),
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
    /// Source file count exceeds the `[source] files` bound.
    TooManyFiles,
    /// One source file exceeds its language's per-file byte bound.
    FileTooLarge,
    /// Aggregate source bytes exceed the `[source] workspace_size` bound.
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
/// known, the underlying cause (I/O, UTF-8, syntax), and - for a `[source]`
/// bound - the typed bound it crossed, boxed so every `Result` carrying this
/// fault inline keeps the size it was sized for.
#[derive(Debug)]
pub struct WorkspaceIndexFault {
    violation: WorkspaceIndexViolation,
    path: Option<PathBuf>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    limit: Option<Box<LimitBreach>>,
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

    /// The provider failure behind this fault, which owns a `Syntax` violation's
    /// registry identity. Only a `Syntax` violation carries a [`SyntaxError`] as
    /// its source, so the downcast alone decides.
    fn syntax_source(&self) -> Option<&SyntaxError> {
        self.source
            .as_deref()
            .and_then(|source| source.downcast_ref::<SyntaxError>())
    }

    /// The Contribution the semantics build refused for one document, behind a
    /// `Provider` violation raised while that document's declarations were
    /// published. Every other provider failure answers `None`.
    fn refused_contribution(&self) -> Option<&ContributionError> {
        self.source
            .as_deref()
            .and_then(|source| source.downcast_ref::<WorkspaceSemanticError>())
            .and_then(WorkspaceSemanticError::refused_contribution)
    }

    /// The warning naming `path` when this fault leaves that one file out of
    /// the index instead of failing the build: a file past the per-file byte
    /// bound, bytes that are not UTF-8, a syntax tree the provider refused
    /// under one of its bounds, or a declaration the Contribution contract
    /// refused. `None` for every fault that fails the build.
    #[must_use]
    pub fn left_out_file(&self, path: ProjectPath) -> Option<WorkspaceIndexWarning> {
        match self.violation {
            WorkspaceIndexViolation::FileTooLarge => {
                Some(WorkspaceIndexWarning::FileTooLarge(path))
            }
            WorkspaceIndexViolation::InvalidSource => {
                Some(WorkspaceIndexWarning::InvalidUtf8Source(path))
            }
            WorkspaceIndexViolation::Syntax => self
                .syntax_source()
                .filter(|error| {
                    error.descriptor().name() == ErrorName::Wire(ErrorCode::LimitExceeded)
                })
                .map(|error| WorkspaceIndexWarning::SyntaxTooLarge {
                    path,
                    violation: error.fault().violation(),
                }),
            WorkspaceIndexViolation::Provider => {
                self.refused_contribution()
                    .map(|error| WorkspaceIndexWarning::Contribution {
                        path,
                        field: error.fault().field(),
                    })
            }
            _ => None,
        }
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
            WorkspaceIndexViolation::Syntax => self.syntax_source().map_or_else(
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
        if let Some(breach) = &self.limit {
            context.extend(breach.context());
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

    fn limit_evidence(&self) -> Option<LimitEvidence> {
        self.limit.as_deref().map(LimitBreach::evidence)
    }
}

/// Opaque workspace indexing failure.
pub type WorkspaceIndexError = Error<WorkspaceIndexFault>;

pub(crate) fn index_error(violation: WorkspaceIndexViolation) -> WorkspaceIndexError {
    Error::new(WorkspaceIndexFault {
        violation,
        path: None,
        source: None,
        limit: None,
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
        limit: None,
    })
}

/// A `[source]` bound refusal: the violation, the path that crossed the bound, and the
/// typed bound - `field`, `observed`, `maximum` - the wire evidence and the rendered
/// context both derive from.
pub(crate) fn index_error_over_limit(
    violation: WorkspaceIndexViolation,
    path: &Path,
    field: &'static str,
    observed: usize,
    maximum: usize,
) -> WorkspaceIndexError {
    Error::new(WorkspaceIndexFault {
        violation,
        path: Some(path.to_path_buf()),
        source: None,
        limit: Some(Box::new(LimitBreach::from_counts(field, observed, maximum))),
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
        limit: None,
    })
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
    /// One catalog file from its path and UTF-8 content, digested over that content.
    pub(crate) fn from_content(path: ProjectPath, content: String) -> Self {
        Self {
            path,
            digest: FileDigest::of(content.as_bytes()),
            content,
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
    /// Returns [`WorkspaceIndexError`] for discovery, read, or configured-bound failures.
    pub fn capture(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
    ) -> Result<Self, WorkspaceIndexError> {
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
        let matcher = PathMatcher::build_with_force_include(
            &root,
            visibility.include(),
            visibility.exclude(),
            visibility.force_include(),
        )?;
        let language = WorkspaceLanguagePolicy::build(&root, languages, text_inclusion)?;
        let gitignore = visibility
            .respect_gitignore()
            .then(|| GitignoreChain::build(&root, limits))
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

    /// Returns every visible regular-file path in project-path order.
    ///
    /// Work is bounded by the configured file-count and directory-depth limits.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] for discovery or configured-bound failures.
    pub fn visible_paths(&self) -> Result<Vec<ProjectPath>, WorkspaceIndexError> {
        let mut paths = Vec::new();
        for entry in source_walk(
            &self.root,
            self.limits.directory_depth_max,
            GitignorePolicy::Ignore,
        ) {
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
            if !file_type.is_some_and(|file_type| file_type.is_file())
                || !self.visible_normalized(entry.path())
            {
                continue;
            }
            if paths.len() >= self.limits.files_max {
                return Err(index_error_over_limit(
                    WorkspaceIndexViolation::TooManyFiles,
                    entry.path(),
                    SOURCE_FILES_FIELD,
                    paths.len().saturating_add(1),
                    self.limits.files_max,
                ));
            }
            let relative = entry.path().strip_prefix(&self.root).map_err(|error| {
                index_error_caused_by(
                    WorkspaceIndexViolation::InvalidPath,
                    Some(entry.path()),
                    error,
                )
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
    /// Returns [`WorkspaceIndexError`] for reads or a file crossing the configured bound.
    pub fn visible_digest(&self, path: &Path) -> Result<Option<FileDigest>, WorkspaceIndexError> {
        if !self.visible(path) {
            return Ok(None);
        }
        let handle = match fs::File::open(path) {
            Ok(handle) => handle,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(index_error_caused_by(
                    WorkspaceIndexViolation::Filesystem,
                    Some(path),
                    error,
                ));
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
                index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
            })?;
        if bytes.len() > self.limits.file_bytes_max {
            return Err(index_error_at(WorkspaceIndexViolation::FileTooLarge, path));
        }
        Ok(Some(FileDigest::of(&bytes)))
    }

    /// Captures every visible regular file's content digest. A file the index leaves out
    /// under the per-file byte bound is absent from the capture, as it is from the index.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] for a read that fails or a walk past the file count
    /// bound.
    pub fn visible_digests(&self) -> Result<WorkspaceDigests, WorkspaceIndexError> {
        let mut digests = Vec::new();
        for path in self.visible_paths()? {
            let absolute = self.root.join(path.as_str());
            match self.visible_digest(&absolute) {
                Ok(Some(digest)) => digests.push((path, digest)),
                Ok(None) => {}
                Err(error) if error.fault().left_out_file(path.clone()).is_some() => {}
                Err(error) => return Err(error),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceIndexWarning {
    /// File bytes are not valid UTF-8.
    InvalidUtf8Source(ProjectPath),
    /// File bytes contain a NUL byte.
    BinarySource(ProjectPath),
    /// File exceeds the configured per-file byte bound.
    FileTooLarge(ProjectPath),
    /// The syntax provider refused the file under one of its bounds.
    SyntaxTooLarge {
        /// The file left out.
        path: ProjectPath,
        /// Which bound the provider's refusal names.
        violation: SyntaxViolation,
    },
    /// The Contribution contract refused one of the file's declarations.
    Contribution {
        /// The file left out.
        path: ProjectPath,
        /// The Contribution field the refusal names.
        field: &'static str,
    },
    /// The workspace publication was full before this file's declarations were offered.
    DeclarationsBeyondBound(ProjectPath),
}

/// Outcome of reading one file into the index: held, or left out with the
/// warning that names it.
enum IndexRead<File> {
    Included(File),
    Skipped(WorkspaceIndexWarning),
}

impl<File> IndexRead<File> {
    /// Leaves one file out, recording the warning that names it once, at the
    /// build that read it.
    fn left_out(warning: WorkspaceIndexWarning) -> Self {
        log_left_out(&warning);
        Self::Skipped(warning)
    }
}

/// Records one file left out of the index, once, at the build that left it out.
fn log_left_out(warning: &WorkspaceIndexWarning) {
    tracing::warn!(
        component = "index",
        operation = "index.build",
        path = warning.path().as_str(),
        reason = %warning.reason(),
        "file left out of the index"
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
}

impl LeftOutFileState {
    fn of(file: &TextSourceFile) -> Self {
        Self {
            content: file.digest(),
            state: FileDigest::of_file_state(file.content().as_bytes(), file.executable()),
        }
    }

    fn of_indexed(file: &IndexedFile) -> Self {
        Self {
            content: file.digest(),
            state: FileDigest::of_file_state(file.source().as_bytes(), file.executable()),
        }
    }
}

/// The files one build holds and the files it left out, gathered before the index is
/// assembled over them.
#[derive(Default)]
pub(crate) struct IndexContents {
    files: BTreeMap<ProjectPath, Arc<IndexedFile>>,
    text_files: BTreeMap<ProjectPath, Arc<TextSourceFile>>,
    left_out: BTreeMap<ProjectPath, LeftOutFileState>,
    warnings: Vec<WorkspaceIndexWarning>,
}

impl IndexContents {
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
    ) -> Result<(), WorkspaceIndexError> {
        match syntax_read(&text_file, context_path, provider, syntax)? {
            IndexRead::Included(file) => {
                self.files.insert(file.path().clone(), Arc::new(file));
                self.hold_text_file(text_file);
            }
            IndexRead::Skipped(warning) => {
                self.left_out
                    .insert(text_file.path().clone(), LeftOutFileState::of(&text_file));
                self.warnings.push(warning);
            }
        }
        Ok(())
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
    /// The file this warning names.
    #[must_use]
    pub fn path(&self) -> &ProjectPath {
        match self {
            Self::InvalidUtf8Source(path)
            | Self::BinarySource(path)
            | Self::FileTooLarge(path)
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
            Self::InvalidUtf8Source(_) => "holds bytes that are not valid UTF-8".to_owned(),
            Self::BinarySource(_) => "contains a NUL byte".to_owned(),
            Self::FileTooLarge(_) => "exceeds the file byte limit".to_owned(),
            Self::SyntaxTooLarge { violation, .. } => {
                format!("exceeds a syntax bound ({})", fault_label(violation))
            }
            Self::Contribution { field, .. } => {
                format!("declares a symbol the Contribution contract refuses ({field})")
            }
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
    text_inclusion: TextFileInclusion,
    fingerprint: WorkspaceFingerprint,
    semantics: WorkspaceSemantics,
    documentation: Arc<DocumentationCollection>,
    /// The documentation layer over `documentation`, built by the first read that projects
    /// onto it; the next publication is a new index and builds its own.
    documentation_layer: OnceLock<Result<DocumentationLayer<'static>, DocumentationError>>,
    notebooks: NotebookFiles,
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
        let classified = rift_core::traced!(component = "index", operation = "index.discover", {
            discover(&root, limits, visibility, &language)
        })?;
        let (files, text_files, left_out, warnings, fingerprint, semantics) =
            rift_core::traced!(component = "index", operation = "index.parse", {
                let mut workspace_bytes = 0_usize;
                let mut contents = IndexContents::default();
                for (path, provider) in classified.source {
                    match read_catalog_file(&root, &path, limits, &mut workspace_bytes)? {
                        IndexRead::Included(text_file) => {
                            contents.hold_source_file(
                                text_file,
                                &path,
                                provider,
                                limits.syntax(),
                            )?;
                        }
                        IndexRead::Skipped(warning) => contents.leave_out(warning),
                    }
                }
                for path in classified.text {
                    match read_catalog_file(&root, &path, limits, &mut workspace_bytes)? {
                        IndexRead::Included(file) => contents.hold_text_file(file),
                        IndexRead::Skipped(warning) => contents.leave_out(warning),
                    }
                }
                let BuiltContents {
                    files,
                    text_files,
                    left_out,
                    warnings,
                    fingerprint,
                    semantics,
                } = built_contents(&root, contents.sorted(), limits.declarations_max(), None)?;
                (
                    files,
                    text_files,
                    left_out,
                    warnings,
                    fingerprint,
                    semantics,
                )
            });
        let declarations = crate::documentation::declarations(&files, &semantics);
        let (documentation, notebooks) = crate::documentation::build(
            &files,
            &text_files,
            &declarations,
            checked_chunk_bytes_max(text_inclusion.chunk_bytes_max()),
            None,
        )?;
        Ok(Self {
            root,
            files,
            text_files,
            left_out,
            composition,
            limits,
            language,
            text_inclusion: text_inclusion.clone(),
            fingerprint,
            semantics,
            documentation: Arc::new(documentation),
            documentation_layer: OnceLock::new(),
            notebooks,
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
        let mut contents = IndexContents::carried_from(self, changes);
        let mut workspace_bytes = Self::indexed_bytes(&contents.files, &contents.text_files);
        for path in changes.indexed() {
            self.read_indexed_path(path, &mut contents, &mut workspace_bytes)?;
        }
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
            checked_chunk_bytes_max(self.text_inclusion.chunk_bytes_max()),
            Some((&self.documentation, &self.notebooks)),
        )?;
        Ok(Self {
            root: self.root.clone(),
            files,
            text_files,
            left_out,
            composition: composition()?,
            limits: self.limits,
            language: Arc::clone(&self.language),
            text_inclusion: self.text_inclusion.clone(),
            fingerprint,
            semantics,
            documentation: Arc::new(documentation),
            documentation_layer: OnceLock::new(),
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
    ) -> Result<(), WorkspaceIndexError> {
        let absolute = self.root.join(path.as_str());
        if !absolute.is_file() {
            return Ok(());
        }
        let Some(class) = self.language.classifies(&absolute)? else {
            return Ok(());
        };
        match read_catalog_file(&self.root, &absolute, self.limits, workspace_bytes)? {
            IndexRead::Included(text_file) => match class {
                ClassifiedPath::Source(provider) => {
                    contents.hold_source_file(
                        text_file,
                        &absolute,
                        provider,
                        self.limits.syntax(),
                    )?;
                }
                ClassifiedPath::Text => contents.hold_text_file(text_file),
            },
            IndexRead::Skipped(warning) => contents.leave_out(warning),
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
    /// directory walk.
    pub(crate) fn from_parts(
        root: PathBuf,
        contents: IndexContents,
        composition: ProviderComposition,
        limits: WorkspaceIndexLimits,
        language: Arc<WorkspaceLanguagePolicy>,
        text_inclusion: TextFileInclusion,
    ) -> Result<Self, WorkspaceIndexError> {
        let BuiltContents {
            files,
            text_files,
            left_out,
            warnings,
            fingerprint,
            semantics,
        } = built_contents(&root, contents.sorted(), limits.declarations_max(), None)?;
        let declarations = crate::documentation::declarations(&files, &semantics);
        let (documentation, notebooks) = crate::documentation::build(
            &files,
            &text_files,
            &declarations,
            checked_chunk_bytes_max(text_inclusion.chunk_bytes_max()),
            None,
        )?;
        Ok(Self {
            root,
            files,
            text_files,
            left_out,
            composition,
            limits,
            language,
            text_inclusion,
            fingerprint,
            semantics,
            documentation: Arc::new(documentation),
            documentation_layer: OnceLock::new(),
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
    pub fn documentation_layer(&self) -> Result<&DocumentationLayer<'static>, &DocumentationError> {
        self.documentation_layer
            .get_or_init(|| DocumentationLayer::shared([Arc::clone(&self.documentation)]))
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
    /// its name, qualified name, derived identifier terms, signature, attached
    /// documentation, and declaration source, and one or more documents per baseline text
    /// file - one whole document when the file is within `[search.text].max_chunk`, one per
    /// chunk otherwise. A chunked file's documents share its real path and share an
    /// identity built from that path plus the chunk index, so a hit still maps back to the
    /// file it came from.
    ///
    /// A fact no provider published stays absent. Nothing substitutes declaration source
    /// into the signature or documentation field, because a reader weighs those fields
    /// apart and would then be weighing the same bytes twice.
    ///
    /// `force_include` files stay outside this derivation: that on-demand walk's contract
    /// covers source units read for one request, not the persistent lexical index.
    #[must_use]
    pub fn index_documents(&self) -> Vec<IndexDocument> {
        rift_core::traced!(
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
                for file in self.text_files() {
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
    /// Returns [`WorkspaceIndexError`] when a matched declaration cannot be read.
    pub fn observe_identifiers(
        &self,
        query: &ParsedQuery,
        bound: usize,
        included: impl Fn(&IndexedFile) -> bool,
        ranking: &mut IdentifierRanking,
    ) -> Result<(), WorkspaceIndexError> {
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
    /// Returns [`WorkspaceIndexError`] when a matched declaration cannot be read.
    pub fn identifier_input(
        &self,
        query: &ParsedQuery,
        bound: usize,
    ) -> Result<RankingInput, WorkspaceIndexError> {
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

    /// Returns the symbol reference adjacency built from this index's normalized graph.
    #[must_use]
    pub const fn relationships(&self) -> &RelationshipStore {
        self.semantics.relationships()
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
        ReadableSymbol::assembled_by(&self.semantics, &identity)
            .ok_or_else(|| provider_error(None, ReadableSymbolMissing { identity }))
    }

    /// Files the build left out of the index, in project-path order.
    ///
    /// Build and rebuild continue after invalid UTF-8, NUL bytes, a file beyond
    /// the configured per-file byte bound, or a syntax tree the provider refuses
    /// under one of its bounds.
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
    /// reaching a text-lane file's bytes directly rather than only through the vector ranking.
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

    fn text_chunk_bytes_max_usize(&self) -> usize {
        checked_chunk_bytes_max(self.text_chunk_bytes_max())
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
            if !matcher.includes(path) {
                continue;
            }
            let Some(ClassifiedPath::Source(provider)) = self.language.classifies(path)? else {
                continue;
            };
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
    ) -> Result<Vec<TextSourceFile>, WorkspaceIndexError> {
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
            Some(path) => Err(index_error_over_limit(
                WorkspaceIndexViolation::TooManyFiles,
                &path,
                FORCE_INCLUDE_FIELD,
                match_count,
                files_max,
            )),
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
    /// Returns `WorkspaceIndexError` when selection, parsing, or bounds fail.
    pub fn force_include_index(
        &self,
        force_include: &[String],
        files_max: usize,
    ) -> Result<Self, WorkspaceIndexError> {
        let text_files = self.force_include_text_files(force_include, files_max)?;
        let mut files = Vec::new();
        for file in &text_files {
            let context_path = self.root.join(file.path().as_str());
            if let Some(ClassifiedPath::Source(provider)) =
                self.language.classifies(&context_path)?
            {
                files.push(indexed_file_from_catalog(
                    file,
                    &context_path,
                    provider,
                    self.limits.syntax(),
                )?);
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
    symbol_matches_where(files, query, limit, |_, _| true)
}

/// The ranking kernel behind [`symbol_matches`], over the declarations `included` accepts.
///
/// The predicate runs before ranking and truncation, so a filtered answer fills
/// `limit` from what it includes; the dependency index passes its public-declaration
/// rule here.
pub(crate) fn symbol_matches_where<'a>(
    files: impl IntoIterator<Item = &'a IndexedFile>,
    query: &str,
    limit: usize,
    included: impl Fn(&IndexedFile, &SyntaxSymbol) -> bool,
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
        .filter(|(file, symbol)| included(file, symbol))
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

/// A provider failure, carrying `path` whenever the failure names one file, so the
/// fault's context reports it and the left-out rule can route it.
fn provider_error(
    path: Option<&Path>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> WorkspaceIndexError {
    index_error_caused_by(WorkspaceIndexViolation::Provider, path, source)
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
/// The graph is built over every held document at once, so a refused Contribution
/// names the document that carried it: the build leaves that file out, keeping its
/// digests as it keeps a file a provider refused, and builds the graph again without
/// it. Every pass either completes or leaves one held file out, so the passes are
/// bounded by the held file count plus the final pass. A failure the left-out rule
/// does not route fails the build with the fault, path attached when known.
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
) -> Result<BuiltContents, WorkspaceIndexError> {
    let passes_max = contents.files.len().saturating_add(1);
    let mut passes = 0_usize;
    loop {
        let fingerprint = WorkspaceFingerprint::from_files(
            &contents.files,
            &contents.text_files,
            &contents.left_out,
        );
        let built = WorkspaceSemantics::build(
            contents.files.values().map(|file| file.syntax()),
            declarations_max,
            fingerprint.revision_number(),
            previous,
        );
        let refused = match built {
            Ok(BuiltSemantics {
                semantics,
                beyond_declaration_bound,
            }) => {
                if !beyond_declaration_bound.is_empty() {
                    passes = passes.saturating_add(1);
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
            Err(error) => error,
        };
        passes = passes.saturating_add(1);
        let path = refused.document_path().cloned();
        let context_path = path.as_ref().map(|path| root.join(path.as_str()));
        let error = provider_error(context_path.as_deref(), refused);
        let left_out = path.and_then(|path| {
            let warning = error.fault().left_out_file(path.clone())?;
            Some((path, warning))
        });
        let Some((path, warning)) = left_out.filter(|_| passes < passes_max) else {
            return Err(error);
        };
        if !contents.leave_out_held(&path, warning) {
            return Err(error);
        }
    }
}

/// Leaves every document the declaration bound had no room for out of `contents`.
///
/// Each one keeps its digests, as every left-out file does, so the next capture of the
/// tree still compares against what this build read. One pass removes every named
/// document, so the pass after it publishes.
///
/// # Errors
///
/// Returns [`WorkspaceIndexError`] when the pass budget is spent or a named document is
/// not held, either of which means the removal cannot make progress.
fn leave_out_beyond_declaration_bound(
    contents: &mut IndexContents,
    beyond: &[ProjectPath],
    within_passes: bool,
) -> Result<(), WorkspaceIndexError> {
    for path in beyond {
        let warning = WorkspaceIndexWarning::DeclarationsBeyondBound(path.clone());
        if !within_passes || !contents.leave_out_held(path, warning) {
            return Err(index_error_at(
                WorkspaceIndexViolation::Provider,
                Path::new(path.as_str()),
            ));
        }
    }
    Ok(())
}

/// Source and text paths [`discover`] found below one root, each list sorted by path.
#[derive(Debug, Default)]
struct DiscoveredPaths {
    /// Each source path with the provider that claimed it, so the caller
    /// never asks the language table the same question twice.
    source: Vec<(PathBuf, &'static dyn SyntaxProvider)>,
    text: Vec<PathBuf>,
}

impl DiscoveredPaths {
    /// Records one classified path against the shared `files_max` budget.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when the budget is already spent, naming the
    /// `[source]` key that owns it and the path that crossed it.
    fn admit(
        &mut self,
        files_max: usize,
        path: &Path,
        class: ClassifiedPath,
    ) -> Result<(), WorkspaceIndexError> {
        let total = self.source.len() + self.text.len();
        if total >= files_max {
            return Err(index_error_over_limit(
                WorkspaceIndexViolation::TooManyFiles,
                path,
                SOURCE_FILES_FIELD,
                total.saturating_add(1),
                files_max,
            ));
        }
        match class {
            ClassifiedPath::Source(provider) => self.source.push((path.to_path_buf(), provider)),
            ClassifiedPath::Text => self.text.push(path.to_path_buf()),
        }
        Ok(())
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
fn discover(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    language: &WorkspaceLanguagePolicy,
) -> Result<DiscoveredPaths, WorkspaceIndexError> {
    let matcher = PathMatcher::build_with_force_include(
        root,
        visibility.include(),
        visibility.exclude(),
        visibility.force_include(),
    )?;
    let gitignore = GitignorePolicy::from_respecting(visibility.respect_gitignore());
    let mut discovered = DiscoveredPaths::default();
    for entry in source_walk(root, limits.directory_depth_max, gitignore) {
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
        discovered.admit(limits.files_max, path, class)?;
    }
    discover_forced(root, limits, &matcher, language, &mut discovered)?;
    discovered
        .source
        .sort_by(|left, right| left.0.cmp(&right.0));
    discovered.text.sort();
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
) -> Result<(), WorkspaceIndexError> {
    let reach = matcher.force_include_reach();
    if reach.is_empty() {
        return Ok(());
    }
    let recorded: HashSet<PathBuf> = discovered
        .source
        .iter()
        .map(|(path, _)| path.clone())
        .chain(discovered.text.iter().cloned())
        .collect();
    for entry in forced_walk(root, limits.directory_depth_max, reach) {
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
        discovered.admit(limits.files_max, path, class)?;
    }
    Ok(())
}

/// The file one walked entry names: `None` for a directory within `directory_depth_max` and
/// for an entry that is neither file nor directory, and a [`WorkspaceIndexViolation::TooDeep`]
/// refusal for a directory past that bound. Both walks report the bound the same way.
fn walked_file(
    entry: &DirEntry,
    directory_depth_max: usize,
) -> Result<Option<&Path>, WorkspaceIndexError> {
    let file_type = entry.file_type();
    if file_type.is_some_and(|file_type| file_type.is_dir()) {
        if entry.depth() > directory_depth_max {
            return Err(index_error_at(
                WorkspaceIndexViolation::TooDeep,
                entry.path(),
            ));
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
                return Err(index_error_over_limit(
                    WorkspaceIndexViolation::TooManyFiles,
                    path,
                    SOURCE_FILES_FIELD,
                    ignore_files.saturating_add(1),
                    limits.files_max,
                ));
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

/// Hashes one already-discovered source and text path set without parsing syntax. Both
/// classes enforce [`WorkspaceIndexLimits::file_bytes_max`] before counting accepted bytes
/// against `workspace_bytes_max`, matching the catalog read the index build applies.
fn capture_paths(
    root: &Path,
    paths: &DiscoveredPaths,
    limits: WorkspaceIndexLimits,
) -> Result<WorkspaceDigests, WorkspaceIndexError> {
    let mut workspace_bytes = 0_usize;
    let source: Vec<PathBuf> = paths.source.iter().map(|(path, _)| path.clone()).collect();
    let source = capture_path_class(&mut workspace_bytes, root, &source, limits)?;
    let text = capture_path_class(&mut workspace_bytes, root, &paths.text, limits)?;
    Ok(WorkspaceDigests::classified(
        source,
        text.into_iter().map(|(path, state, _)| (path, state)),
    ))
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

/// Reads one path class into captured file states: each kept file's project path, its
/// file-state digest, and its content digest, in walk order.
fn capture_path_class(
    workspace_bytes: &mut usize,
    root: &Path,
    paths: &[PathBuf],
    limits: WorkspaceIndexLimits,
) -> Result<Vec<(ProjectPath, FileDigest, FileDigest)>, WorkspaceIndexError> {
    let mut captured = Vec::with_capacity(paths.len());
    for path in paths {
        let handle = fs::File::open(path).map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        let metadata = handle.metadata().map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        let bytes = read_file_bytes(handle, path, limits)?;
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
            return Err(index_error_over_limit(
                WorkspaceIndexViolation::WorkspaceTooLarge,
                path,
                SOURCE_WORKSPACE_SIZE_FIELD,
                *workspace_bytes,
                limits.workspace_bytes_max(),
            ));
        }
        let project_path = project_path_below(root, path)?;
        let (content, state) =
            FileDigest::of_content_and_file_state(&bytes, metadata_is_executable(&metadata));
        captured.push((project_path, state, content));
    }
    Ok(captured)
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
    let bytes = read_file_bytes(handle, path, limits)?;
    let project_path = project_path_below(root, path)?;
    let mut file = included_file(project_path, bytes, path, provider, limits, workspace_bytes)?;
    file.set_executable(metadata_is_executable(&metadata));
    Ok(file)
}

/// Reads one cataloged file's syntax facts, leaving the file out when the
/// provider refuses it under one of its bounds.
fn syntax_read(
    file: &TextSourceFile,
    context_path: &Path,
    provider: &dyn SyntaxProvider,
    limits: SyntaxLimits,
) -> Result<IndexRead<IndexedFile>, WorkspaceIndexError> {
    match indexed_file_from_catalog(file, context_path, provider, limits) {
        Ok(indexed) => Ok(IndexRead::Included(indexed)),
        Err(error) => match error.fault().left_out_file(file.path().clone()) {
            Some(warning) => Ok(IndexRead::left_out(warning)),
            None => Err(error),
        },
    }
}

pub(crate) fn indexed_file_from_catalog(
    file: &TextSourceFile,
    context_path: &Path,
    provider: &dyn SyntaxProvider,
    limits: SyntaxLimits,
) -> Result<IndexedFile, WorkspaceIndexError> {
    let syntax = provider
        .analyze(
            SyntaxSource {
                path: file.path(),
                text: file.content(),
            },
            limits,
        )
        .map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Syntax, Some(context_path), error)
        })?;
    Ok(IndexedFile::new(
        file.path().clone(),
        file.content().to_owned(),
        file.digest(),
        file.executable(),
        syntax,
    ))
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
        return Err(index_error_over_limit(
            WorkspaceIndexViolation::WorkspaceTooLarge,
            context_path,
            SOURCE_WORKSPACE_SIZE_FIELD,
            *workspace_bytes,
            limits.workspace_bytes_max,
        ));
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
            index_error_caused_by(WorkspaceIndexViolation::Syntax, Some(context_path), error)
        })?;
    let digest = FileDigest::of(source.as_bytes());
    Ok(IndexedFile::new(
        project_path,
        source,
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
) -> Result<IndexRead<TextSourceFile>, WorkspaceIndexError> {
    let handle = fs::File::open(path).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let metadata = handle.metadata().map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let bytes = read_file_bytes(handle, path, limits)?;
    let project_path = project_path_below(root, path)?;
    if bytes.len() > limits.file_bytes_max() {
        return Ok(IndexRead::left_out(WorkspaceIndexWarning::FileTooLarge(
            project_path,
        )));
    }
    if bytes.contains(&0) {
        return Ok(IndexRead::left_out(WorkspaceIndexWarning::BinarySource(
            project_path,
        )));
    }
    if std::str::from_utf8(&bytes).is_err() {
        return Ok(IndexRead::left_out(
            WorkspaceIndexWarning::InvalidUtf8Source(project_path),
        ));
    }
    let mut file = included_text_file(project_path, bytes, path, limits, workspace_bytes)?;
    file.executable = metadata_is_executable(&metadata);
    Ok(IndexRead::Included(file))
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

/// Reads at most one byte beyond the file bound, so callers can classify an oversized
/// file without reading its remaining bytes. Capture, catalog, and syntax reads share it.
fn read_file_bytes(
    reader: impl std::io::Read,
    path: &Path,
    limits: WorkspaceIndexLimits,
) -> Result<Vec<u8>, WorkspaceIndexError> {
    let mut bytes = Vec::new();
    reader
        .take(limits.file_bytes_max().saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
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
) -> Result<TextSourceFile, WorkspaceIndexError> {
    *workspace_bytes = workspace_bytes
        .checked_add(bytes.len())
        .ok_or_else(|| index_error_at(WorkspaceIndexViolation::WorkspaceTooLarge, context_path))?;
    if *workspace_bytes > limits.workspace_bytes_max {
        return Err(index_error_over_limit(
            WorkspaceIndexViolation::WorkspaceTooLarge,
            context_path,
            SOURCE_WORKSPACE_SIZE_FIELD,
            *workspace_bytes,
            limits.workspace_bytes_max,
        ));
    }
    let content = source_utf8(bytes, context_path)?;
    Ok(TextSourceFile::from_content(project_path, content))
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

pub(crate) fn relative_path(path: &Path) -> Result<ProjectPath, WorkspaceIndexError> {
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

/// The class one declaration's names reach against a lowercase query.
///
/// The classing itself lives in `rift-ranking`, so a package index, an
/// in-memory fixture, and this index all order a declaration the same way.
fn symbol_rank(symbol: &SyntaxSymbol, query: &str) -> Option<IdentifierMatchClass> {
    match_class(
        query,
        &symbol.name.to_lowercase(),
        &symbol.qualified_name.to_lowercase(),
    )
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
/// no signature leaves that field absent rather than filling it with the declaration's
/// source, so the two are weighed apart.
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
        declaration_fields(file, symbol),
        left_out,
    )
}

/// The searchable fields one declaration fills, wherever it was read from.
///
/// A package index publishes the same fields for its own declarations; only the identity
/// and the address differ. Sharing the derivation is what makes the two comparable:
/// equal bytes produce equal fields and one digest, whichever index published them.
#[must_use]
pub(crate) fn declaration_fields(file: &IndexedFile, symbol: &SyntaxSymbol) -> DocumentFields {
    let source = declaration_source(file, symbol.range);
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
        .with(SearchableField::DeclarationSource, source)
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
/// searchable corpus is absent, and the log says which path lost it. A package index
/// leaves an oversized document out the same way.
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
    pub(crate) fn record(&mut self, path: &ProjectPath, error: &rift_ranking::RankingError) {
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
        tracing::warn!(
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
/// and its file name so a hit still maps back to the file it came from.
fn push_text_documents(
    documents: &mut Vec<IndexDocument>,
    file: &TextSourceFile,
    chunk_bytes_max: u64,
    left_out: &mut LeftOut,
) {
    let name = file_name(file.path());
    if !exceeds_chunk_bound(file.content().len(), chunk_bytes_max) {
        documents.extend(text_document(
            file.path().as_str().to_owned(),
            file,
            name.as_deref(),
            file.content(),
            left_out,
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
        documents.extend(text_document(
            identity,
            file,
            name.as_deref(),
            chunk.content(),
            left_out,
        ));
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::TooManyFiles
        );
        assert!(
            error
                .fault()
                .path()
                .is_some_and(|path| path.ends_with("b.rs")),
            "the refusal names the first path past the bound: {:?}",
            error.fault().path()
        );
        assert_eq!(
            error.fault().limit_evidence().map(|evidence| (
                evidence.field,
                evidence.limit,
                evidence.required
            )),
            Some(("paths.force_include".to_owned(), 1, 3))
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::TooManyFiles
        );
        let evidence = error.fault().limit_evidence().expect("limit evidence");
        assert_eq!((evidence.limit, evidence.required), (1, 2));
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::SourcePatternInvalid
        );
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

        let decomposed = directory.path().join("src/cafe\u{301}.rs");
        fs::write(&decomposed, "fn accent() {}").expect("decomposed source");
        let limits = WorkspaceIndexLimits::default();
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
        let provider = registry::provider_for_language(&rift_protocol::read::Language {
            name: "rust".to_owned(),
            dialect: None,
        })
        .expect("the shipped Rust provider");
        DiscoveredPaths {
            source: source.into_iter().map(|path| (path, provider)).collect(),
            text: Vec::new(),
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::Filesystem
        );
        assert!(error.to_string().contains("failed.rs"), "{error}");
        assert!(
            rift_core::causes(&error)
                .iter()
                .any(|cause| cause.contains("capture read failed"))
        );
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
            &TextFileInclusion::from(&configuration.search),
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
            &TextFileInclusion::from(&configuration.search),
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
            &TextFileInclusion::from(&configuration.search),
            &LanguageFileSelections::from(&configuration),
        )
        .expect_err("a path two entries claim must refuse the candidate");

        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::LanguageMatchConflict
        );
        let message = error.to_string();
        assert!(
            message.contains("rust") && message.contains("python"),
            "the refusal names both entries: {message}"
        );
        assert!(
            error
                .fault()
                .context()
                .iter()
                .any(|entry| entry.value().contains("lib.rs")),
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
        assert_eq!(
            index.warnings(),
            &[WorkspaceIndexWarning::SyntaxTooLarge {
                path: deep,
                violation: SyntaxViolation::TooDeep,
            }]
        );
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
        assert_eq!(
            deep.warnings(),
            &[WorkspaceIndexWarning::SyntaxTooLarge {
                path: lib_path.clone(),
                violation: SyntaxViolation::TooDeep,
            }]
        );

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
        assert_eq!(
            index.warnings(),
            &[WorkspaceIndexWarning::Contribution {
                path: wide.clone(),
                field: "provider_symbol",
            }]
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
        assert_eq!(
            wide.warnings(),
            &[WorkspaceIndexWarning::Contribution {
                path: lib_path.clone(),
                field: "provider_symbol",
            }]
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

    /// A `Provider` fault leaves a file out only when one document's Contribution was
    /// refused; a provider fault raised anywhere else fails the build.
    #[test]
    fn test_left_out_file_names_a_refused_contribution_and_no_other_provider_fault() {
        let path = ProjectPath::new("src/wide.rs").expect("path");
        let refused = rift_core::SourceRange::new(1, 0).expect_err("a reversed range is refused");
        let field = refused.fault().field();
        let error = provider_error(
            Some(Path::new("/workspace/src/wide.rs")),
            WorkspaceSemanticError::Document {
                path: path.clone(),
                error: rift_syntax::SyntaxPublicationError::Contribution(refused),
            },
        );
        assert_eq!(
            error.fault().path(),
            Some(Path::new("/workspace/src/wide.rs")),
            "the fault carries the document's path"
        );
        assert_eq!(
            error.fault().left_out_file(path.clone()),
            Some(WorkspaceIndexWarning::Contribution {
                path: path.clone(),
                field,
            })
        );

        let elsewhere = provider_error(
            None,
            WorkspaceSemanticError::Normalization(
                rift_core::SourceRange::new(1, 0).expect_err("a reversed range is refused"),
            ),
        );
        assert_eq!(elsewhere.fault().left_out_file(path), None);
    }

    #[test]
    fn test_left_out_file_names_the_per_file_faults_and_no_other() {
        let path = ProjectPath::new("src/deep.rs").expect("path");
        let context = Path::new("src/deep.rs");
        let warning_for = |violation| {
            index_error_at(violation, context)
                .fault()
                .left_out_file(path.clone())
        };
        assert_eq!(
            warning_for(WorkspaceIndexViolation::FileTooLarge),
            Some(WorkspaceIndexWarning::FileTooLarge(path.clone()))
        );
        assert_eq!(
            warning_for(WorkspaceIndexViolation::InvalidSource),
            Some(WorkspaceIndexWarning::InvalidUtf8Source(path.clone()))
        );
        assert_eq!(
            warning_for(WorkspaceIndexViolation::WorkspaceTooLarge),
            None
        );
        assert_eq!(warning_for(WorkspaceIndexViolation::Filesystem), None);
        assert_eq!(
            warning_for(WorkspaceIndexViolation::Syntax),
            None,
            "a syntax fault with no provider refusal behind it fails the build"
        );

        let strict = SyntaxLimits::new(1, 1, 1).expect("positive bounds");
        let text = TextSourceFile::from_content(path.clone(), "pub fn deep() {}\n".to_owned());
        let refused =
            indexed_file_from_catalog(&text, context, &RustSyntaxProvider::default(), strict)
                .expect_err("the syntax byte bound refuses the file");
        assert_eq!(
            refused.fault().left_out_file(path.clone()),
            Some(WorkspaceIndexWarning::SyntaxTooLarge {
                path,
                violation: SyntaxViolation::SourceTooLarge,
            })
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
        ) -> Result<SyntaxDocument, SyntaxError> {
            Err(rift_core::Error::new(
                rift_syntax::SyntaxFault::UnknownNodeKind {
                    kind: "beacon".to_owned(),
                },
            ))
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

        assert_eq!(error.fault().violation(), WorkspaceIndexViolation::Syntax);
        assert_eq!(error.fault().left_out_file(path), None);
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::SourcePatternInvalid
        );
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
            update.fields().get(SearchableField::DeclarationSource),
            Some("pub fn update() {}")
        );
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
            error.fault().violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::Filesystem
        );
        assert!(
            error
                .fault()
                .path()
                .is_some_and(|path| path.ends_with("sealed.txt")),
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
        assert_eq!(
            large.fault().violation(),
            WorkspaceIndexViolation::FileTooLarge
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
            assert_eq!(
                error.fault().violation(),
                WorkspaceIndexViolation::ZeroLimit
            );
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
        assert_eq!(
            files.fault().violation(),
            WorkspaceIndexViolation::TooManyFiles
        );
        let rendered = files.to_string();
        assert!(
            rendered.contains("field source.files") && rendered.contains("maximum 1"),
            "{rendered}"
        );
        assert_eq!(
            files
                .fault()
                .limit_evidence()
                .map(|evidence| evidence.field),
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
            bytes.fault().violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge
        );
        let rendered = bytes.to_string();
        assert!(
            rendered.contains("field source.workspace_size") && rendered.contains("maximum 10"),
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
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidSource
        );
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
            content: "hello".to_owned(),
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
            content,
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
            content: "hello".to_owned(),
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
            )
            .expect("the capture must read the tree")
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
