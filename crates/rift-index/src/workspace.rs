use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, Match, Walk, WalkBuilder};
use rift_core::constants::{
    READ_RESULTS_MAX_DEFAULT, WORKSPACE_BYTES_MAX_DEFAULT, WORKSPACE_CONFIGURATION_FILE,
    WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT, WORKSPACE_FILES_MAX_DEFAULT,
    WORKSPACE_IGNORED_DIRECTORIES,
};
use rift_core::{
    CompositionId, Error, ErrorCode, ErrorContext, ErrorName, Fault, ProjectPath, ProviderId,
    SourceVisibility, TextFileInclusion, fault_label,
};
use rift_provider::{Component, CompositionBuilder, ProviderComposition};
use rift_syntax::{
    SyntaxDocument, SyntaxError, SyntaxNode, SyntaxProvider, SyntaxSource, SyntaxSymbol, registry,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::chunk::text_chunks;
use crate::glob::PathMatcher;
use crate::lexical::{LexicalUnit, LexicalUnitKind};

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
    pub(crate) const fn files_max(self) -> usize {
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
    /// Composition recipe failed validation.
    Composition,
    /// Requested result bound exceeds configured maximum.
    ResultLimit,
    /// A `source.include` or `source.exclude` entry is not a valid glob.
    SourcePatternInvalid,
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
            | WorkspaceIndexViolation::SourcePatternInvalid => {
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

/// One immutable indexed Rust file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    path: ProjectPath,
    source: String,
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

    /// Returns syntax facts.
    #[must_use]
    pub const fn syntax(&self) -> &SyntaxDocument {
        &self.syntax
    }
}

/// One immutable indexed non-source text file: a path and its whole content, with no syntax
/// facts. A file over `[search.text].max_chunk` still lands here whole; chunking only
/// happens when [`WorkspaceIndex::lexical_units`] derives search units from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSourceFile {
    path: ProjectPath,
    content: String,
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
}

/// Symbol plus source file matched by read index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolMatch<'a> {
    /// Containing file.
    pub file: &'a IndexedFile,
    /// Matched declaration.
    pub symbol: &'a SyntaxSymbol,
    /// Stable semantic match priority.
    pub rank: SymbolMatchRank,
}

/// Stable semantic priority for symbol-name matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SymbolMatchRank {
    /// Query equals complete qualified name.
    QualifiedExact,
    /// Query equals short declaration name.
    NameExact,
    /// Complete qualified name ends with query.
    QualifiedSuffix,
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
    gitignore: Option<Gitignore>,
    text_inclusion: TextFileInclusion,
}

/// Separates one path from its source bytes in workspace identity material.
const FINGERPRINT_PATH_SEPARATOR: u8 = 0;
/// Separates adjacent files in workspace identity material.
const FINGERPRINT_FILE_SEPARATOR: u8 = 0xff;

impl WorkspaceFingerprint {
    /// Captures visible source and included text paths and bytes without parsing syntax.
    ///
    /// Work is bounded by [`WorkspaceIndexLimits`].
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] for discovery, read, encoding, or
    /// configured-bound failures.
    pub fn capture(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
    ) -> Result<Self, WorkspaceIndexError> {
        let root = canonical_root(root)?;
        let classified = discover(&root, limits, visibility, text_inclusion)?;
        fingerprint_paths(&root, &classified, limits)
    }

    fn from_files(files: &[IndexedFile], text_files: &[TextSourceFile]) -> Self {
        let mut hasher = Sha256::new();
        for file in files {
            update_fingerprint(&mut hasher, file.path(), file.source().as_bytes());
        }
        for file in text_files {
            update_fingerprint(&mut hasher, file.path(), file.content().as_bytes());
        }
        Self(hasher.finalize().into())
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
        let watched_root = root.to_path_buf();
        let root = canonical_root(root)?;
        let matcher = PathMatcher::build(&root, visibility.include(), visibility.exclude())?;
        let gitignore = visibility
            .respect_gitignore()
            .then(|| build_gitignore(&root, limits))
            .transpose()?;
        Ok(Self {
            root,
            watched_root,
            matcher,
            gitignore,
            text_inclusion: text_inclusion.clone(),
        })
    }

    /// Returns whether one source or included-text path passes current workspace policy.
    /// Source and text-file candidates share this one predicate: only the extension gate
    /// that includes a path in either class differs between them.
    #[must_use]
    pub fn includes(&self, path: &Path) -> bool {
        let Some(path) = self.normalized_path(path) else {
            return false;
        };
        let path = path.as_ref();
        let extension_included = has_source_extension(path) || self.text_inclusion.includes(path);
        let above_hard_floor = hard_floor_includes_path(&self.root, path);
        if !extension_included || !above_hard_floor {
            return false;
        }
        let configuration_includes = self.matcher.includes(path);
        let gitignore_includes = self.gitignore.as_ref().is_none_or(|gitignore| {
            !matches!(
                gitignore.matched_path_or_any_parents(path, false),
                Match::Ignore(_)
            )
        });
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
        let gitignore_includes = self.gitignore.as_ref().is_none_or(|gitignore| {
            !matches!(
                gitignore.matched_path_or_any_parents(path, true),
                Match::Ignore(_)
            )
        });
        configuration_includes && gitignore_includes
    }

    /// Returns whether path identifies root workspace configuration file.
    #[must_use]
    pub fn is_workspace_configuration(&self, path: &Path) -> bool {
        self.normalized_path(path)
            .is_some_and(|path| path == self.root.join(WORKSPACE_CONFIGURATION_FILE))
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

/// Immutable current-workspace Rust read index.
#[derive(Debug)]
pub struct WorkspaceIndex {
    root: PathBuf,
    files: Vec<IndexedFile>,
    text_files: Vec<TextSourceFile>,
    composition: ProviderComposition,
    limits: WorkspaceIndexLimits,
    text_chunk_bytes_max: u64,
    fingerprint: WorkspaceFingerprint,
}

impl WorkspaceIndex {
    /// Scans current Rust files and included text files directly from workspace root,
    /// applying `visibility`'s `.gitignore` and `[source]` include/exclude policy, and
    /// `text_inclusion`'s extension list, on top of the hard floor.
    ///
    /// Symlinks, `.git`, `.rift`, and `target` are never followed or
    /// indexed, whatever `visibility` says.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] for invalid root, I/O, syntax, an invalid
    /// `[source]` pattern, or an exceeded bound.
    pub fn build(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
    ) -> Result<Self, WorkspaceIndexError> {
        let root = canonical_root(root)?;
        let composition = composition()?;
        let classified = discover(&root, limits, visibility, text_inclusion)?;
        let mut workspace_bytes = 0_usize;
        let mut files = Vec::with_capacity(classified.source.len());
        for path in classified.source {
            let provider = syntax_provider_for(&path);
            let file = read_file(&root, &path, provider, limits, &mut workspace_bytes)?;
            files.push(file);
        }
        let mut text_files = Vec::with_capacity(classified.text.len());
        for path in classified.text {
            let file = read_text_file(&root, &path, limits, &mut workspace_bytes)?;
            text_files.push(file);
        }
        let fingerprint = WorkspaceFingerprint::from_files(&files, &text_files);
        Ok(Self {
            root,
            files,
            text_files,
            composition,
            limits,
            text_chunk_bytes_max: text_inclusion.chunk_bytes_max(),
            fingerprint,
        })
    }

    /// Assembles an index from files another source already accepted - the
    /// revision build, whose bytes come from git objects instead of a
    /// directory walk. Revision reads carry no text files: `text_chunk_bytes_max` is kept
    /// only so a future revision-text feature can reuse this constructor unchanged.
    pub(crate) fn from_parts(
        root: PathBuf,
        files: Vec<IndexedFile>,
        text_files: Vec<TextSourceFile>,
        composition: ProviderComposition,
        limits: WorkspaceIndexLimits,
        text_chunk_bytes_max: u64,
    ) -> Self {
        let fingerprint = WorkspaceFingerprint::from_files(&files, &text_files);
        Self {
            root,
            files,
            text_files,
            composition,
            limits,
            text_chunk_bytes_max,
            fingerprint,
        }
    }

    /// Returns canonical real workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns deterministic project-path ordered files.
    #[must_use]
    pub fn files(&self) -> &[IndexedFile] {
        &self.files
    }

    /// Returns deterministic project-path ordered included text files.
    #[must_use]
    pub fn text_files(&self) -> &[TextSourceFile] {
        &self.text_files
    }

    /// Derives lexical search units from this index: one unit per indexed symbol, carrying
    /// its declaration source, and one or more units per included text file - one whole unit
    /// when the file is within `[search.text].max_chunk`, one unit per chunk otherwise. A
    /// chunked file's units share its real path and share an identity built from that path
    /// plus the chunk index, so a hit still maps back to the file it came from.
    ///
    /// `force_include` files stay outside this derivation: that on-demand walk's contract
    /// covers source units read for one request, not the persistent lexical index.
    #[must_use]
    pub fn lexical_units(&self) -> Vec<LexicalUnit> {
        let mut units = Vec::with_capacity(self.files.len() + self.text_files.len());
        for file in &self.files {
            for symbol in file.syntax().symbols() {
                units.push(symbol_lexical_unit(file, symbol));
            }
        }
        for file in &self.text_files {
            push_text_lexical_units(&mut units, file, self.text_chunk_bytes_max);
        }
        units
    }

    /// Returns each text file split into more than one lexical chunk, paired with its chunk
    /// count, so a caller can report the split instead of the index silently absorbing it.
    #[must_use]
    pub fn chunked_text_files(&self) -> Vec<(ProjectPath, usize)> {
        self.text_files
            .iter()
            .filter(|file| exceeds_chunk_bound(file.content().len(), self.text_chunk_bytes_max))
            .map(|file| {
                let chunks = text_chunks(
                    file.content(),
                    checked_chunk_bytes_max(self.text_chunk_bytes_max),
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

    /// Returns the maximum result count accepted per query against this index.
    #[must_use]
    pub const fn results_max(&self) -> usize {
        self.limits.results_max()
    }

    /// Finds declarations by exact, suffix, or substring name.
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
        Ok(symbol_matches(&self.files, query, limit))
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
        Ok(source_line_matches(&self.files, query, limit))
    }

    /// Returns file by canonical project path.
    #[must_use]
    pub fn file(&self, path: &ProjectPath) -> Option<&IndexedFile> {
        self.files.iter().find(|file| file.path() == path)
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
    /// This walk covers source units only: `force_include`'s contract is reading a source
    /// declaration on demand for one request, not including text files in the persistent
    /// lexical index, so a `[search.text]` extension never joins it.
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

    fn validate_result_limit(&self, limit: usize) -> Result<(), WorkspaceIndexError> {
        if limit == 0 || limit > self.limits.results_max {
            return Err(index_error(WorkspaceIndexViolation::ResultLimit));
        }
        Ok(())
    }
}

/// Declaration matches for `query` across `files`, ranked qualified-exact first, then
/// name-exact, qualified-suffix, and substring. Shared so an on-demand file set (search's
/// `force_include`) scores identically to the index.
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
    let query = query.to_lowercase();
    let mut matches = Vec::new();
    for file in files {
        for (line_index, line) in file.source().lines().enumerate() {
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

/// A candidate path's included granularity: a provider-declared source extension always wins
/// over a `[search.text]` extension when both sets would include it.
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

/// Classifies `path` by extension alone, before any visibility policy is consulted: a
/// provider-declared source extension is [`PathClass::Source`]; otherwise an extension
/// [`TextFileInclusion`] includes is [`PathClass::Text`]; anything else is not a candidate.
fn classify_path(path: &Path, text_inclusion: &TextFileInclusion) -> Option<PathClass> {
    if has_source_extension(path) {
        Some(PathClass::Source)
    } else if text_inclusion.includes(path) {
        Some(PathClass::Text)
    } else {
        None
    }
}

/// Source and included text paths visible below `root`: the hard floor (`.git`, `.rift`,
/// `target`, symlinks) is always applied, `visibility.respect_gitignore()` then layers the
/// workspace's own `.gitignore` chain, and `visibility.include()`/`.exclude()` narrow or drop
/// candidate files. A path is classified by extension first - source wins over text when an
/// extension is included by both - and the same visibility policy then applies to whichever
/// class it landed in, so a text file reaches the same `.gitignore` and `[source]` policy a
/// source file does.
///
/// Directories are walked in file-name order so a bound violation is reported
/// deterministically; both returned lists are sorted by path. Source and text paths share one
/// `files_max` budget, counted as they are discovered.
fn discover(
    root: &Path,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    text_inclusion: &TextFileInclusion,
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
        let Some(class) = classify_path(path, text_inclusion) else {
            continue;
        };
        if !matcher.includes(path) {
            continue;
        }
        let total = discovered.source.len() + discovered.text.len();
        if total >= limits.files_max {
            return Err(index_error_at(WorkspaceIndexViolation::TooManyFiles, path));
        }
        match class {
            PathClass::Source => discovered.source.push(path.to_path_buf()),
            PathClass::Text => discovered.text.push(path.to_path_buf()),
        }
    }
    discovered.source.sort();
    discovered.text.sort();
    Ok(discovered)
}

/// Compiles bounded workspace `.gitignore` chain for direct event matching.
fn build_gitignore(
    root: &Path,
    limits: WorkspaceIndexLimits,
) -> Result<Gitignore, WorkspaceIndexError> {
    let mut builder = GitignoreBuilder::new(root);
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
        if let Some(error) = builder.add(path) {
            return Err(index_error_caused_by(
                WorkspaceIndexViolation::Filesystem,
                Some(path),
                error,
            ));
        }
    }
    builder.build().map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(root), error)
    })
}

/// Hashes one already-discovered source and text path set without parsing syntax. Source
/// paths enforce [`WorkspaceIndexLimits::file_bytes_max`] per file, matching the bound the
/// index build applies; text paths carry no per-file bound, matching
/// [`included_text_file`] - both classes still count against the shared aggregate
/// `workspace_bytes_max`.
fn fingerprint_paths(
    root: &Path,
    paths: &DiscoveredPaths,
    limits: WorkspaceIndexLimits,
) -> Result<WorkspaceFingerprint, WorkspaceIndexError> {
    let mut hasher = Sha256::new();
    let mut workspace_bytes = 0_usize;
    absorb_fingerprint_paths(
        &mut hasher,
        &mut workspace_bytes,
        root,
        &paths.source,
        limits,
        PathClass::Source,
    )?;
    absorb_fingerprint_paths(
        &mut hasher,
        &mut workspace_bytes,
        root,
        &paths.text,
        limits,
        PathClass::Text,
    )?;
    Ok(WorkspaceFingerprint(hasher.finalize().into()))
}

/// Reads and hashes one path list into the running fingerprint, applying the per-file byte
/// bound only to [`PathClass::Source`] paths.
fn absorb_fingerprint_paths(
    hasher: &mut Sha256,
    workspace_bytes: &mut usize,
    root: &Path,
    paths: &[PathBuf],
    limits: WorkspaceIndexLimits,
    class: PathClass,
) -> Result<(), WorkspaceIndexError> {
    for path in paths {
        let bytes = fs::read(path).map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
        })?;
        if class == PathClass::Source && bytes.len() > limits.file_bytes_max() {
            return Err(index_error_at(WorkspaceIndexViolation::FileTooLarge, path));
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
        let relative = path.strip_prefix(root).map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::InvalidPath, Some(path), error)
        })?;
        let project_path = relative_path(relative)?;
        let content = String::from_utf8(bytes).map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::InvalidSource, Some(path), error)
        })?;
        update_fingerprint(hasher, &project_path, content.as_bytes());
    }
    Ok(())
}

/// Adds one unambiguous project-path/source pair to workspace identity.
fn update_fingerprint(hasher: &mut Sha256, path: &ProjectPath, source: &[u8]) {
    hasher.update(path.as_str().as_bytes());
    hasher.update([FINGERPRINT_PATH_SEPARATOR]);
    hasher.update(source);
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

/// The hard floor every workspace applies before `.gitignore` or
/// `[source]` are consulted: `.git`, `.rift`, and `target` are never
/// descended into, and a symlink is never followed or indexed.
fn hard_floor_includes(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if entry.path_is_symlink() {
        return false;
    }
    let is_dir = entry
        .file_type()
        .is_some_and(|file_type| file_type.is_dir());
    !(is_dir && is_ignored_directory(entry.file_name()))
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

/// The registered provider for one path the extension gate already accepted.
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

fn is_ignored_directory(name: &OsStr) -> bool {
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
    let bytes = fs::read(path).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let relative = path.strip_prefix(root).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::InvalidPath, Some(path), error)
    })?;
    let project_path = relative_path(relative)?;
    included_file(project_path, bytes, path, provider, limits, workspace_bytes)
}

/// Includes one source file's bytes in an index, whatever supplied them: the
/// per-file and aggregate byte bounds, UTF-8, and the syntax parse are the
/// same for a directory walk and a committed revision tree. `context_path`
/// names the file in refusals.
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
    let source = String::from_utf8(bytes).map_err(|error| {
        index_error_caused_by(
            WorkspaceIndexViolation::InvalidSource,
            Some(context_path),
            error,
        )
    })?;
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
        source,
        syntax,
    })
}

fn read_text_file(
    root: &Path,
    path: &Path,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<TextSourceFile, WorkspaceIndexError> {
    let bytes = fs::read(path).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    let relative = path.strip_prefix(root).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::InvalidPath, Some(path), error)
    })?;
    let project_path = relative_path(relative)?;
    included_text_file(project_path, bytes, path, limits, workspace_bytes)
}

/// Includes one text file's bytes in an index: unlike [`included_file`], there is no
/// per-file byte bound - a text file joins the lexical index whole, however large, and
/// [`WorkspaceIndex::lexical_units`] splits an oversized one into chunks rather than the
/// index refusing it. The aggregate `workspace_bytes_max` bound still applies, and UTF-8
/// validity is still required, matching a source file's `InvalidSource` refusal.
/// `context_path` names the file in refusals.
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
    let content = String::from_utf8(bytes).map_err(|error| {
        index_error_caused_by(
            WorkspaceIndexViolation::InvalidSource,
            Some(context_path),
            error,
        )
    })?;
    Ok(TextSourceFile {
        path: project_path,
        content,
    })
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
    if qualified == query {
        SymbolMatchRank::QualifiedExact
    } else if symbol.name.to_lowercase() == query {
        SymbolMatchRank::NameExact
    } else if qualified.ends_with(query) {
        SymbolMatchRank::QualifiedSuffix
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
/// [`rift_core::rust_symbol_identity`] the read service uses for that declaration's wire
/// `SymbolId`, so a lexical hit's identity equals the id `get_symbol` returns for it.
fn symbol_lexical_unit(file: &IndexedFile, symbol: &SyntaxSymbol) -> LexicalUnit {
    let identity = rift_core::rust_symbol_identity(file.path().as_str(), &symbol.qualified_name);
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
        fs::write(directory.path().join("README.md"), "ignored").expect("fixture prose");
        directory
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
        assert_eq!(index.files().len(), 1);
        assert_eq!(index.composition().steps().len(), 3);
        let symbols = index.symbols("update", 5).expect("bounded symbol read");
        assert_eq!(symbols[0].symbol.qualified_name, "Rift::update");
        let source = index.source_matches("pub struct", 5).expect("lexical read");
        assert_eq!(source[0].1, 1);
        let path = ProjectPath::new("src/lib.rs").expect("fixture path");
        assert!(!index.nodes(&path, 4).expect("indexed path").is_empty());
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

        assert!(policy.includes(&directory.path().join("src/lib.rs")));
        assert!(!policy.includes(&directory.path().join("src/ignored.rs")));
        assert!(!policy.includes(&directory.path().join("src/generated/code.rs")));
        assert!(!policy.includes(&directory.path().join("target/code.rs")));
        assert!(
            !policy.includes(&directory.path().join("src/logo.png")),
            "an extension neither a source provider nor [search.text] includes must be refused"
        );
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
    fn test_workspace_source_policy_includes_text_extensions_under_the_same_visibility_rules() {
        // A `[search.text]` extension must reach the same visibility predicate a source
        // extension does: `.gitignore` and `[source]` exclude a text candidate exactly as
        // they would a source one.
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir_all(directory.path().join("docs/generated")).expect("directories");
        fs::write(directory.path().join(".gitignore"), "docs/ignored.md\n").expect("ignore file");
        fs::write(directory.path().join("docs/guide.md"), "guide").expect("guide");
        fs::write(directory.path().join("docs/ignored.md"), "ignored").expect("ignored");
        fs::write(directory.path().join("docs/generated/gen.md"), "generated").expect("generated");
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

        assert!(policy.includes(&directory.path().join("docs/guide.md")));
        assert!(policy.includes(&directory.path().join("notes.txt")));
        assert!(
            !policy.includes(&directory.path().join("docs/ignored.md")),
            "gitignore must hide a text candidate exactly as it would a source one"
        );
        assert!(
            !policy.includes(&directory.path().join("docs/generated/gen.md")),
            "[source] exclude must hide a text candidate exactly as it would a source one"
        );
        assert!(
            !policy.includes(&directory.path().join("logo.png")),
            "an extension outside both included sets must be refused"
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
            index.files().len(),
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
            .force_include_files(&[".git/**".to_owned()], 10)
            .expect("force_include walk");
        assert!(
            floor_reach.is_empty(),
            "the hard floor must stay unreachable via force_include"
        );

        let bound_error = index
            .force_include_files(&["*.rs".to_owned()], 1)
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
        assert_eq!(index.files().len(), 1);
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
        assert!(index.files().is_empty());
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
        let file_error = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::new(2, 4, 100, 4, 5).expect("positive limits"),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect_err("file byte bound");
        assert_eq!(
            file_error.fault().violation(),
            WorkspaceIndexViolation::FileTooLarge
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
            index.symbols("::update", 5).expect("suffix match")[0].rank,
            SymbolMatchRank::QualifiedSuffix
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

        fs::write(directory.path().join("src/invalid.rs"), [0xff]).expect("invalid UTF-8");
        assert_eq!(
            WorkspaceIndex::build(
                directory.path(),
                limits,
                &SourceVisibility::default(),
                &rift_core::TextFileInclusion::default()
            )
            .expect_err("invalid source")
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
    /// tests exercising [`fingerprint_paths`] directly.
    fn source_only(source: Vec<PathBuf>) -> DiscoveredPaths {
        DiscoveredPaths {
            source,
            text: Vec::new(),
        }
    }

    #[test]
    fn test_fingerprint_paths_preserves_bound_and_path_failures() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let limits = WorkspaceIndexLimits::new(5, 8, 10, 4, 5).expect("limits");

        let missing = root.join("missing.rs");
        let error = fingerprint_paths(&root, &source_only(vec![missing]), limits)
            .expect_err("missing source");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::Filesystem
        );

        let oversized = root.join("oversized.rs");
        fs::write(&oversized, b"123456789").expect("oversized source");
        let error = fingerprint_paths(&root, &source_only(vec![oversized]), limits)
            .expect_err("file bound");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::FileTooLarge
        );

        let first = root.join("first.rs");
        let second = root.join("second.rs");
        fs::write(&first, b"123456").expect("first source");
        fs::write(&second, b"123456").expect("second source");
        let error = fingerprint_paths(&root, &source_only(vec![first, second]), limits)
            .expect_err("workspace bound");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge
        );

        let outside = tempfile::NamedTempFile::new().expect("outside source");
        fs::write(outside.path(), b"fn x(){}").expect("outside bytes");
        let error = fingerprint_paths(
            &root,
            &source_only(vec![outside.path().to_path_buf()]),
            limits,
        )
        .expect_err("outside path");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidPath
        );

        let invalid = root.join("invalid.rs");
        fs::write(&invalid, [0xff]).expect("invalid source");
        let error = fingerprint_paths(&root, &source_only(vec![invalid]), limits)
            .expect_err("invalid UTF-8");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidSource
        );
    }

    #[test]
    fn test_fingerprint_paths_text_class_has_no_per_file_bound_but_counts_toward_workspace_bound() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        // file_bytes_max is 4: a text file far larger must not refuse as FileTooLarge, and
        // workspace_bytes_max is generous so only the per-file bound is under test here.
        let limits = WorkspaceIndexLimits::new(5, 4, 1_000, 4, 5).expect("limits");
        let big_text = root.join("big.md");
        fs::write(&big_text, b"much larger than four bytes").expect("big text file");
        let paths = DiscoveredPaths {
            source: Vec::new(),
            text: vec![big_text],
        };
        fingerprint_paths(&root, &paths, limits)
            .expect("a text file over file_bytes_max must not refuse");

        let tight = WorkspaceIndexLimits::new(5, 4, 10, 4, 5).expect("limits");
        let over_workspace = root.join("over.md");
        fs::write(&over_workspace, b"still more than ten bytes total").expect("oversized text");
        let paths = DiscoveredPaths {
            source: Vec::new(),
            text: vec![over_workspace],
        };
        let error = fingerprint_paths(&root, &paths, tight)
            .expect_err("the aggregate workspace bound still applies to text files");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge
        );
    }

    #[test]
    fn test_fingerprint_paths_text_class_still_refuses_invalid_utf8() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let limits = WorkspaceIndexLimits::default();
        let invalid = root.join("invalid.md");
        fs::write(&invalid, [0xff, 0xfe]).expect("invalid text bytes");
        let paths = DiscoveredPaths {
            source: Vec::new(),
            text: vec![invalid],
        };
        let error =
            fingerprint_paths(&root, &paths, limits).expect_err("invalid UTF-8 text must refuse");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidSource
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
    fn test_classify_path_source_wins_when_an_extension_is_included_by_both_sets() {
        // A source provider's extension always wins over a configured text extension when
        // the same extension appears in both sets.
        let inclusion = TextFileInclusion::new(vec!["rs".to_owned()], 1_024);
        assert_eq!(
            classify_path(Path::new("lib.rs"), &inclusion),
            Some(PathClass::Source)
        );
    }

    #[test]
    fn test_classify_path_returns_text_for_an_included_non_source_extension() {
        let inclusion = TextFileInclusion::new(vec!["md".to_owned()], 1_024);
        assert_eq!(
            classify_path(Path::new("readme.md"), &inclusion),
            Some(PathClass::Text)
        );
    }

    #[test]
    fn test_classify_path_returns_none_for_an_extension_outside_both_sets() {
        let inclusion = TextFileInclusion::new(vec!["md".to_owned()], 1_024);
        assert_eq!(classify_path(Path::new("logo.png"), &inclusion), None);
    }

    #[test]
    fn test_build_includes_text_files_behind_gitignore_and_source_excludes() {
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
            .iter()
            .map(|file| file.path().as_str())
            .collect();
        assert_eq!(text_paths, ["docs/guide.md", "docs/notes.mdx"]);
    }

    #[test]
    fn test_build_refuses_invalid_utf8_text_file() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("invalid.md"), [0xff, 0xfe]).expect("invalid text bytes");
        let error = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect_err("invalid UTF-8 text must refuse");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::InvalidSource
        );
    }

    #[test]
    fn test_build_text_files_count_toward_the_shared_file_count_bound() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        fs::write(directory.path().join("a.rs"), "pub fn a() {}\n").expect("source");
        fs::write(directory.path().join("b.md"), "text body").expect("text");
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
        fs::write(directory.path().join("readme.md"), "hello").expect("text file");
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("workspace index");
        assert_eq!(index.text_files().len(), 1);
        assert_eq!(index.text_files()[0].path().as_str(), "readme.md");
        assert_eq!(index.text_files()[0].content(), "hello");
    }

    #[test]
    fn test_fingerprint_changes_when_a_text_files_bytes_change_or_it_appears_or_disappears() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = TextFileInclusion::default();

        let empty =
            WorkspaceFingerprint::capture(directory.path(), limits, &visibility, &inclusion)
                .expect("fingerprint of an empty workspace");

        fs::write(directory.path().join("readme.md"), "first").expect("text file");
        let appeared =
            WorkspaceFingerprint::capture(directory.path(), limits, &visibility, &inclusion)
                .expect("fingerprint after the text file appears");
        assert_ne!(
            empty, appeared,
            "a newly appeared text file must change the fingerprint"
        );

        fs::write(directory.path().join("readme.md"), "second").expect("edited text file");
        let edited =
            WorkspaceFingerprint::capture(directory.path(), limits, &visibility, &inclusion)
                .expect("fingerprint after the text file's bytes change");
        assert_ne!(
            appeared, edited,
            "an edited text file's bytes must change the fingerprint"
        );

        fs::remove_file(directory.path().join("readme.md")).expect("remove text file");
        let removed =
            WorkspaceFingerprint::capture(directory.path(), limits, &visibility, &inclusion)
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
        fs::write(directory.path().join("guide.md"), "guide body").expect("text file");
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
        assert_eq!(text_unit.identity(), "guide.md");
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
        fs::write(directory.path().join("big.md"), content).expect("oversized text file");
        let inclusion = TextFileInclusion::new(vec!["md".to_owned()], 10);
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
        assert_eq!(identities, ["big.md#0", "big.md#1", "big.md#2", "big.md#3"]);
        let rejoined: String = units.iter().map(LexicalUnit::content).collect();
        assert_eq!(
            rejoined, content,
            "chunk content must reconstruct the file exactly"
        );
        for unit in &units {
            assert_eq!(unit.name(), Some("big"));
            assert_eq!(unit.path().as_str(), "big.md");
        }

        let chunked = index.chunked_text_files();
        assert_eq!(chunked.len(), 1);
        assert_eq!(chunked[0].0.as_str(), "big.md");
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
    #[should_panic(expected = "a text file's lexical identity must be non-empty")]
    fn test_push_text_lexical_units_of_root_path_panics_on_empty_identity() {
        // `ProjectPath::new("")` is valid and names the workspace root, so a whole-file text
        // unit for it builds its identity from that empty path, which `LexicalUnit::new`
        // refuses: this invariant must never fire for a real discovered path.
        let file = TextSourceFile {
            path: ProjectPath::new("").expect("an empty project path names the workspace root"),
            content: "hello".to_owned(),
        };
        let mut units = Vec::new();
        push_text_lexical_units(&mut units, &file, 1_024);
    }
}
