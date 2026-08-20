use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rift_core::constants::{
    READ_RESULTS_MAX_DEFAULT, RUST_SOURCE_BYTES_MAX_DEFAULT, WORKSPACE_BYTES_MAX_DEFAULT,
    WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT, WORKSPACE_FILES_MAX_DEFAULT,
    WORKSPACE_IGNORED_DIRECTORIES,
};
use rift_core::{
    CompositionId, ErrorContext, ErrorDescriptor, ErrorName, ErrorRegistry, ProjectPath,
    ProviderId, render_failure,
};
use rift_provider::{Component, CompositionBuilder, ProviderComposition};
use rift_syntax::{
    RustNode, RustSource, RustSymbol, RustSyntaxDocument, RustSyntaxError, RustSyntaxProvider,
};

#[derive(Debug)]
struct WorkspaceFiles;
#[derive(Debug)]
struct RustFacts;
#[derive(Debug)]
struct ReadIndex;

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
}

impl Default for WorkspaceIndexLimits {
    fn default() -> Self {
        Self {
            files_max: WORKSPACE_FILES_MAX_DEFAULT,
            file_bytes_max: RUST_SOURCE_BYTES_MAX_DEFAULT,
            workspace_bytes_max: WORKSPACE_BYTES_MAX_DEFAULT,
            directory_depth_max: WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT,
            results_max: READ_RESULTS_MAX_DEFAULT,
        }
    }
}

/// Stable workspace indexing failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Rust source is not UTF-8.
    InvalidSource,
    /// Filesystem operation failed.
    Filesystem,
    /// Rust syntax analysis failed.
    Syntax,
    /// Composition recipe failed validation.
    Composition,
    /// Requested result bound exceeds configured maximum.
    ResultLimit,
}

/// Opaque workspace indexing failure.
#[derive(Debug)]
pub struct WorkspaceIndexError {
    violation: WorkspaceIndexViolation,
    path: Option<PathBuf>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl WorkspaceIndexError {
    fn new(violation: WorkspaceIndexViolation) -> Self {
        Self {
            violation,
            path: None,
            source: None,
        }
    }

    fn at(violation: WorkspaceIndexViolation, path: &Path) -> Self {
        Self {
            violation,
            path: Some(path.to_path_buf()),
            source: None,
        }
    }

    fn caused_by(
        violation: WorkspaceIndexViolation,
        path: Option<&Path>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            violation,
            path: path.map(Path::to_path_buf),
            source: Some(Box::new(source)),
        }
    }

    /// Returns stable failure classification.
    #[must_use]
    pub const fn violation(&self) -> WorkspaceIndexViolation {
        self.violation
    }

    /// Returns involved filesystem path when available.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Returns canonical registry metadata for this failure.
    ///
    /// A syntax failure delegates to the underlying [`RustSyntaxError`]'s
    /// descriptor when the source downcasts to one.
    #[must_use]
    pub fn descriptor(&self) -> ErrorDescriptor {
        match self.violation {
            WorkspaceIndexViolation::ZeroLimit
            | WorkspaceIndexViolation::InvalidRoot
            | WorkspaceIndexViolation::Composition => {
                ErrorRegistry::descriptor(ErrorName::ConfigurationInvalid)
            }
            WorkspaceIndexViolation::TooDeep
            | WorkspaceIndexViolation::TooManyFiles
            | WorkspaceIndexViolation::FileTooLarge
            | WorkspaceIndexViolation::WorkspaceTooLarge
            | WorkspaceIndexViolation::ResultLimit => {
                ErrorRegistry::descriptor(ErrorName::LimitExceeded)
            }
            WorkspaceIndexViolation::InvalidPath => {
                ErrorRegistry::descriptor(ErrorName::UnsupportedPath)
            }
            WorkspaceIndexViolation::InvalidSource => {
                ErrorRegistry::descriptor(ErrorName::ContentUnavailable)
            }
            WorkspaceIndexViolation::Filesystem => {
                ErrorRegistry::descriptor(ErrorName::StorageFailure)
            }
            WorkspaceIndexViolation::Syntax => self
                .source
                .as_deref()
                .and_then(|source| source.downcast_ref::<RustSyntaxError>())
                .map_or_else(
                    || ErrorRegistry::descriptor(ErrorName::InternalError),
                    RustSyntaxError::descriptor,
                ),
        }
    }

    /// Returns ordered typed context: violation label, then path when present.
    #[must_use]
    pub fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new(
            "violation",
            violation_label(self.violation),
        )];
        if let Some(path) = &self.path {
            context.push(ErrorContext::new("path", path.display().to_string()));
        }
        context
    }
}

const fn violation_label(violation: WorkspaceIndexViolation) -> &'static str {
    match violation {
        WorkspaceIndexViolation::ZeroLimit => "zero_limit",
        WorkspaceIndexViolation::InvalidRoot => "invalid_root",
        WorkspaceIndexViolation::TooDeep => "too_deep",
        WorkspaceIndexViolation::TooManyFiles => "too_many_files",
        WorkspaceIndexViolation::FileTooLarge => "file_too_large",
        WorkspaceIndexViolation::WorkspaceTooLarge => "workspace_too_large",
        WorkspaceIndexViolation::InvalidPath => "invalid_path",
        WorkspaceIndexViolation::InvalidSource => "invalid_source",
        WorkspaceIndexViolation::Filesystem => "filesystem",
        WorkspaceIndexViolation::Syntax => "syntax",
        WorkspaceIndexViolation::Composition => "composition",
        WorkspaceIndexViolation::ResultLimit => "result_limit",
    }
}

// Failure context is carried by the error itself: the offending path renders
// as named context and the underlying cause (I/O, UTF-8, syntax) stays on
// Error::source.
impl fmt::Display for WorkspaceIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&render_failure(self.descriptor(), &self.context()))
    }
}

impl std::error::Error for WorkspaceIndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// One immutable indexed Rust file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    path: ProjectPath,
    source: String,
    syntax: RustSyntaxDocument,
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
    pub const fn syntax(&self) -> &RustSyntaxDocument {
        &self.syntax
    }
}

/// Symbol plus source file matched by read index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolMatch<'a> {
    /// Containing file.
    pub file: &'a IndexedFile,
    /// Matched declaration.
    pub symbol: &'a RustSymbol,
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

/// Immutable current-workspace Rust read index.
#[derive(Debug)]
pub struct WorkspaceIndex {
    root: PathBuf,
    files: Vec<IndexedFile>,
    composition: ProviderComposition,
    limits: WorkspaceIndexLimits,
}

impl WorkspaceIndex {
    /// Scans current Rust files directly from workspace root.
    ///
    /// Symlinks and Rift state are never followed or indexed.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] for invalid root, I/O, syntax, or exceeded bound.
    pub fn build(root: &Path, limits: WorkspaceIndexLimits) -> Result<Self, WorkspaceIndexError> {
        let root = canonical_root(root)?;
        let composition = composition()?;
        let paths = discover(&root, limits)?;
        let parser = RustSyntaxProvider::default();
        let mut workspace_bytes = 0_usize;
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let file = read_file(&root, &path, &parser, limits, &mut workspace_bytes)?;
            files.push(file);
        }
        Ok(Self {
            root,
            files,
            composition,
            limits,
        })
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

    /// Returns typed provider recipe used for this index.
    #[must_use]
    pub const fn composition(&self) -> &ProviderComposition {
        &self.composition
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
        let query = query.to_lowercase();
        let mut matches = self
            .files
            .iter()
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
        Ok(matches)
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
        let query = query.to_lowercase();
        let mut matches = Vec::new();
        for file in &self.files {
            for (line_index, line) in file.source().lines().enumerate() {
                if line.to_lowercase().contains(&query) {
                    matches.push((file, line_index + 1, line.into()));
                    if matches.len() == limit {
                        return Ok(matches);
                    }
                }
            }
        }
        Ok(matches)
    }

    /// Returns file by canonical project path.
    #[must_use]
    pub fn file(&self, path: &ProjectPath) -> Option<&IndexedFile> {
        self.files.iter().find(|file| file.path() == path)
    }

    /// Returns syntax nodes covering byte position.
    #[must_use]
    pub fn nodes(&self, path: &ProjectPath, position: u64) -> Option<Vec<&RustNode>> {
        self.file(path).map(|file| file.syntax().nodes_at(position))
    }

    fn validate_result_limit(&self, limit: usize) -> Result<(), WorkspaceIndexError> {
        if limit == 0 || limit > self.limits.results_max {
            return Err(WorkspaceIndexError::new(
                WorkspaceIndexViolation::ResultLimit,
            ));
        }
        Ok(())
    }
}

fn positive_bound(bound: usize) -> Result<(), WorkspaceIndexError> {
    if bound == 0 {
        return Err(WorkspaceIndexError::new(WorkspaceIndexViolation::ZeroLimit));
    }
    Ok(())
}

fn canonical_root(root: &Path) -> Result<PathBuf, WorkspaceIndexError> {
    let canonical = fs::canonicalize(root).map_err(|error| {
        WorkspaceIndexError::caused_by(WorkspaceIndexViolation::InvalidRoot, Some(root), error)
    })?;
    if !canonical.is_dir() {
        return Err(WorkspaceIndexError::at(
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

fn component<Input: 'static, Output: 'static>(
    id: &str,
) -> Result<Component<Input, Output>, WorkspaceIndexError> {
    Ok(Component::new(
        ProviderId::new(id).map_err(composition_error)?,
    ))
}

fn composition_error(
    source: impl std::error::Error + Send + Sync + 'static,
) -> WorkspaceIndexError {
    WorkspaceIndexError::caused_by(WorkspaceIndexViolation::Composition, None, source)
}

fn discover(
    root: &Path,
    limits: WorkspaceIndexLimits,
) -> Result<Vec<PathBuf>, WorkspaceIndexError> {
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    let mut files = Vec::new();
    while let Some((directory, depth)) = pending.pop() {
        if depth > limits.directory_depth_max {
            return Err(WorkspaceIndexError::at(
                WorkspaceIndexViolation::TooDeep,
                &directory,
            ));
        }
        let mut entries = fs::read_dir(&directory)
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(|error| {
                WorkspaceIndexError::caused_by(
                    WorkspaceIndexViolation::Filesystem,
                    Some(&directory),
                    error,
                )
            })?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries.into_iter().rev() {
            discover_entry(&entry, depth, &mut pending, &mut files, limits)?;
        }
    }
    files.sort();
    Ok(files)
}

fn discover_entry(
    entry: &fs::DirEntry,
    depth: usize,
    pending: &mut Vec<(PathBuf, usize)>,
    files: &mut Vec<PathBuf>,
    limits: WorkspaceIndexLimits,
) -> Result<(), WorkspaceIndexError> {
    let path = entry.path();
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        WorkspaceIndexError::caused_by(WorkspaceIndexViolation::Filesystem, Some(&path), error)
    })?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        if !is_ignored_directory(&entry.file_name()) {
            pending.push((path, depth + 1));
        }
    } else if metadata.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
        if files.len() >= limits.files_max {
            return Err(WorkspaceIndexError::at(
                WorkspaceIndexViolation::TooManyFiles,
                &path,
            ));
        }
        files.push(path);
    }
    Ok(())
}

fn is_ignored_directory(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| WORKSPACE_IGNORED_DIRECTORIES.contains(&name))
}

fn read_file(
    root: &Path,
    path: &Path,
    parser: &RustSyntaxProvider,
    limits: WorkspaceIndexLimits,
    workspace_bytes: &mut usize,
) -> Result<IndexedFile, WorkspaceIndexError> {
    let bytes = fs::read(path).map_err(|error| {
        WorkspaceIndexError::caused_by(WorkspaceIndexViolation::Filesystem, Some(path), error)
    })?;
    if bytes.len() > limits.file_bytes_max {
        return Err(WorkspaceIndexError::at(
            WorkspaceIndexViolation::FileTooLarge,
            path,
        ));
    }
    *workspace_bytes = workspace_bytes
        .checked_add(bytes.len())
        .ok_or_else(|| WorkspaceIndexError::at(WorkspaceIndexViolation::WorkspaceTooLarge, path))?;
    if *workspace_bytes > limits.workspace_bytes_max {
        return Err(WorkspaceIndexError::at(
            WorkspaceIndexViolation::WorkspaceTooLarge,
            path,
        ));
    }
    let source = String::from_utf8(bytes).map_err(|error| {
        WorkspaceIndexError::caused_by(WorkspaceIndexViolation::InvalidSource, Some(path), error)
    })?;
    let relative = path.strip_prefix(root).map_err(|error| {
        WorkspaceIndexError::caused_by(WorkspaceIndexViolation::InvalidPath, Some(path), error)
    })?;
    let project_path = relative_path(relative)?;
    let syntax = parser
        .analyze(RustSource {
            path: &project_path,
            text: &source,
        })
        .map_err(|error| {
            WorkspaceIndexError::caused_by(WorkspaceIndexViolation::Syntax, Some(path), error)
        })?;
    Ok(IndexedFile {
        path: project_path,
        source,
        syntax,
    })
}

fn relative_path(path: &Path) -> Result<ProjectPath, WorkspaceIndexError> {
    let value = path
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| WorkspaceIndexError::at(WorkspaceIndexViolation::InvalidPath, path))?
        .join("/");
    ProjectPath::new(value).map_err(|error| {
        WorkspaceIndexError::caused_by(WorkspaceIndexViolation::InvalidPath, Some(path), error)
    })
}

fn symbol_rank(symbol: &RustSymbol, query: &str) -> SymbolMatchRank {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rift_syntax::RustSyntaxLimits;
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
        let index = WorkspaceIndex::build(directory.path(), WorkspaceIndexLimits::default())
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
        let index = WorkspaceIndex::build(directory.path(), WorkspaceIndexLimits::default())
            .expect("workspace index");
        assert!(index.symbols("escaped", 5).expect("symbol read").is_empty());
        assert!(index.symbols("hidden", 5).expect("symbol read").is_empty());
    }

    #[test]
    fn test_index_enforces_file_workspace_depth_and_result_bounds() {
        let directory = fixture();
        let file_error = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::new(2, 4, 100, 4, 5).expect("positive limits"),
        )
        .expect_err("file byte bound");
        assert_eq!(
            file_error.violation(),
            WorkspaceIndexViolation::FileTooLarge
        );

        let index = WorkspaceIndex::build(directory.path(), WorkspaceIndexLimits::default())
            .expect("workspace index");
        assert_eq!(
            index
                .symbols("Rift", index.limits.results_max() + 1)
                .expect_err("result bound")
                .violation(),
            WorkspaceIndexViolation::ResultLimit,
        );
        assert_eq!(
            index
                .source_matches("Rift", 0)
                .expect_err("zero result bound")
                .violation(),
            WorkspaceIndexViolation::ResultLimit,
        );
    }

    #[test]
    fn test_index_enforces_scan_bounds_and_root_contract() {
        assert_eq!(
            WorkspaceIndexLimits::new(0, 1, 1, 1, 1)
                .expect_err("zero bound")
                .violation(),
            WorkspaceIndexViolation::ZeroLimit,
        );

        let missing = PathBuf::from("missing-rift-workspace");
        let missing_error = WorkspaceIndex::build(&missing, WorkspaceIndexLimits::default())
            .expect_err("missing root");
        assert_eq!(
            missing_error.violation(),
            WorkspaceIndexViolation::InvalidRoot
        );
        assert_eq!(missing_error.path(), Some(missing.as_path()));
        assert!(std::error::Error::source(&missing_error).is_some());

        let directory = fixture();
        let file_root = directory.path().join("src/lib.rs");
        assert_eq!(
            WorkspaceIndex::build(&file_root, WorkspaceIndexLimits::default())
                .expect_err("file root")
                .violation(),
            WorkspaceIndexViolation::InvalidRoot,
        );

        fs::write(directory.path().join("src/other.rs"), "fn other() {}").expect("second source");
        assert_eq!(
            WorkspaceIndex::build(
                directory.path(),
                WorkspaceIndexLimits::new(1, 1_000, 2_000, 4, 5).expect("limits"),
            )
            .expect_err("file count bound")
            .violation(),
            WorkspaceIndexViolation::TooManyFiles,
        );
        assert_eq!(
            WorkspaceIndex::build(
                directory.path(),
                WorkspaceIndexLimits::new(5, 1_000, 8, 4, 5).expect("limits"),
            )
            .expect_err("workspace byte bound")
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
            )
            .expect_err("depth bound")
            .violation(),
            WorkspaceIndexViolation::TooDeep,
        );
    }

    #[test]
    fn test_index_queries_cover_rank_and_early_limit_paths() {
        let directory = fixture();
        let index = WorkspaceIndex::build(directory.path(), WorkspaceIndexLimits::default())
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
        ];
        for (violation, message) in cases {
            assert_eq!(WorkspaceIndexError::new(violation).to_string(), message);
        }
    }

    #[test]
    fn test_error_display_appends_offending_path() {
        let error = WorkspaceIndexError::at(
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
        assert_eq!(error.violation(), WorkspaceIndexViolation::Composition);
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn test_read_file_classifies_syntax_and_non_nfc_path_failures() {
        let directory = fixture();
        let limits = WorkspaceIndexLimits::default();
        let mut bytes = 0;
        let strict_parser =
            RustSyntaxProvider::new(RustSyntaxLimits::new(1, 1, 1).expect("positive bounds"));
        let source_path = directory.path().join("src/lib.rs");
        let syntax_error = read_file(
            directory.path(),
            &source_path,
            &strict_parser,
            limits,
            &mut bytes,
        )
        .expect_err("syntax byte bound");
        assert_eq!(syntax_error.violation(), WorkspaceIndexViolation::Syntax);
        assert_eq!(syntax_error.path(), Some(source_path.as_path()));
        assert!(std::error::Error::source(&syntax_error).is_some());

        let parser = RustSyntaxProvider::default();
        let decomposed = directory.path().join("src/cafe\u{301}.rs");
        fs::write(&decomposed, "fn accent() {}").expect("decomposed source");
        let path_error = read_file(directory.path(), &decomposed, &parser, limits, &mut bytes)
            .expect_err("non-NFC project path");
        assert_eq!(path_error.violation(), WorkspaceIndexViolation::InvalidPath);
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
        let unreadable = WorkspaceIndex::build(directory.path(), WorkspaceIndexLimits::default())
            .expect_err("unreadable directory");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restore read");
        assert_eq!(unreadable.violation(), WorkspaceIndexViolation::Filesystem);
        assert_eq!(unreadable.path(), Some(locked.as_path()));

        let unsearchable = root.join("unsearchable");
        fs::create_dir(&unsearchable).expect("unsearchable directory");
        fs::write(unsearchable.join("entry.rs"), "fn entry() {}").expect("entry source");
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o444))
            .expect("remove search");
        let stat_error = WorkspaceIndex::build(directory.path(), WorkspaceIndexLimits::default())
            .expect_err("unsearchable directory");
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o755))
            .expect("restore search");
        assert_eq!(stat_error.violation(), WorkspaceIndexViolation::Filesystem);
        assert_eq!(
            stat_error.path(),
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
                .violation(),
            WorkspaceIndexViolation::Filesystem,
        );

        let outside = tempfile::tempdir().expect("outside directory");
        let outside_file = outside.path().join("outside.rs");
        fs::write(&outside_file, "fn outside() {}").expect("outside source");
        assert_eq!(
            read_file(directory.path(), &outside_file, &parser, limits, &mut bytes,)
                .expect_err("outside project path")
                .violation(),
            WorkspaceIndexViolation::InvalidPath,
        );

        fs::write(directory.path().join("src/invalid.rs"), [0xff]).expect("invalid UTF-8");
        assert_eq!(
            WorkspaceIndex::build(directory.path(), limits)
                .expect_err("invalid source")
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
            .violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge,
        );
    }
}
