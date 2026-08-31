use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use rift_core::ProjectPath as CoreProjectPath;
use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_core::{
    Error, ErrorCode, ErrorContext, ErrorName, Fault, LanguageFileSelections, SourceVisibility,
    TextFileInclusion,
};
use rift_history::{HistoryError, Repository};
use rift_index::{
    BindingPolicy, FileDigest, IndexedFile, PathChanges, ReadableSymbol, SymbolMatch,
    WorkspaceDigests, WorkspaceFingerprint, WorkspaceIndex, WorkspaceIndexError,
    WorkspaceIndexLimits, WorkspaceIndexWarning, WorkspaceSourcePolicy,
};
use rift_protocol::configuration::HistoryConfiguration;
use rift_protocol::read::{
    ContributionKey, Digest, ExactKind, Extensions, FileId, GetSymbolHit, GetSymbolParams,
    GetSymbolResult, Language, Node, NodeFacet, NodeId, NodesParams, NodesResult, Pagination,
    ProjectPath, ReadWarning, RevisionId, SourceExcerpt, SourceUnitId, SourceUnitSpan, Symbol,
    SymbolDisagreement, SymbolId, SymbolOrigin, SymbolPresentationField, SymbolResolution,
    TextRange,
};
use rift_syntax::{ByteRange, SyntaxNode, SyntaxProvider, SyntaxSymbol, registry};
use sha2::{Digest as _, Sha256};

use crate::history::SymbolTimelines;

/// One read-service failure: what was asked, and why it cannot be served.
///
/// An indexing failure keeps the classification of the
/// [`WorkspaceIndexError`] it wraps, so a crossed parse bound surfaces as
/// `limit_exceeded` rather than as an unclassified failure.
#[derive(Debug)]
pub enum ReadFault {
    /// Workspace could not be indexed.
    Index(WorkspaceIndexError),
    /// The workspace's version control could not serve the requested revision.
    History(HistoryError),
    /// A language engine failed while serving the request.
    Engine(rift_lsp::session::EngineError),
    /// Request uses functionality this release does not serve, where configuring the
    /// workspace could serve it.
    Unsupported {
        /// The unserved capability the request named.
        capability: String,
    },
    /// A path's extension carries no shipped syntax provider, so no workspace
    /// configuration can ever serve a syntax read at this path.
    UnclaimedExtension {
        /// The extension named, or a statement that the path carries none.
        extension: String,
    },
    /// Request is invalid for direct workspace reads.
    Invalid {
        /// The rejected request field.
        field: &'static str,
        /// The rule the field's value broke.
        violation: String,
    },
    /// Requested source does not exist.
    NotFound {
        /// The path the request addressed.
        path: String,
    },
    /// A claimed path's bytes are not valid UTF-8: the path is real, but the index could
    /// not read it and holds no file there.
    SourceUnavailable {
        /// The path the request addressed.
        path: String,
    },
    /// Workspace files could not be read or written.
    Storage {
        /// The path being read or written.
        path: String,
        /// The filesystem operation that failed.
        operation: &'static str,
        /// The rendered I/O failure.
        io: String,
    },
    /// Tokio could not run or join one bounded blocking operation.
    Task {
        /// Operation submitted to the blocking executor.
        operation: &'static str,
        /// Runtime failure account.
        detail: String,
    },
    /// Current workspace index cannot be validated within its deadline.
    Unavailable {
        /// Operation waiting for a validated state.
        operation: &'static str,
        /// Bounded failure account.
        detail: String,
    },
    /// A blocking operation waited past its configured queue bound.
    CapacityTimeout {
        /// Operation waiting for blocking capacity.
        operation: &'static str,
        /// Configured queue wait bound in milliseconds.
        timeout_ms: u64,
    },
}

impl Fault for ReadFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::Index(source) => source.descriptor().name(),
            Self::History(source) => source.descriptor().name(),
            Self::Engine(source) => source.name(),
            Self::Unsupported { .. } | Self::UnclaimedExtension { .. } => {
                ErrorName::Wire(ErrorCode::CapabilityUnavailable)
            }
            Self::Invalid { .. } => ErrorName::Wire(ErrorCode::InvalidRequest),
            Self::NotFound { .. } => ErrorName::Wire(ErrorCode::ResourceNotFound),
            Self::SourceUnavailable { .. } => ErrorName::Wire(ErrorCode::ContentUnavailable),
            Self::Storage { .. } => ErrorName::Wire(ErrorCode::StorageFailure),
            Self::Task { .. } => ErrorName::Wire(ErrorCode::InternalError),
            Self::Unavailable { .. } | Self::CapacityTimeout { .. } => {
                ErrorName::Wire(ErrorCode::TemporarilyUnavailable)
            }
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::Index(source) => source.context(),
            Self::History(source) => source.context(),
            Self::Engine(source) => source.context(),
            Self::Unsupported { capability } => {
                vec![ErrorContext::new("capability", capability.clone())]
            }
            Self::UnclaimedExtension { extension } => {
                vec![ErrorContext::new("capability", extension.clone())]
            }
            Self::Invalid { field, violation } => vec![
                ErrorContext::new("field", *field),
                ErrorContext::new("violation", violation.clone()),
            ],
            Self::NotFound { path } | Self::SourceUnavailable { path } => {
                vec![ErrorContext::new("path", path.clone())]
            }
            Self::Storage {
                path,
                operation,
                io,
            } => vec![
                ErrorContext::new("path", path.clone()),
                ErrorContext::new("operation", *operation),
                ErrorContext::new("io", io.clone()),
            ],
            Self::Task { operation, detail } | Self::Unavailable { operation, detail } => vec![
                ErrorContext::new("operation", *operation),
                ErrorContext::new("detail", detail.clone()),
            ],
            Self::CapacityTimeout {
                operation,
                timeout_ms,
            } => vec![
                ErrorContext::new("operation", *operation),
                ErrorContext::new("timeout_ms", timeout_ms.to_string()),
            ],
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Index(source) => Some(source),
            Self::History(source) => Some(source),
            Self::Engine(source) => Some(source),
            Self::Unsupported { .. }
            | Self::UnclaimedExtension { .. }
            | Self::Invalid { .. }
            | Self::NotFound { .. }
            | Self::SourceUnavailable { .. }
            | Self::Storage { .. }
            | Self::Task { .. }
            | Self::Unavailable { .. }
            | Self::CapacityTimeout { .. } => None,
        }
    }

    /// A path whose extension no shipped provider parses can never be served: unlike
    /// [`Self::Unsupported`], which also classifies a capability the operator could turn
    /// on, no `rift.toml` table adds a syntax grammar this release does not ship.
    fn action_override(&self) -> Option<&'static str> {
        match self {
            Self::UnclaimedExtension { .. } => Some("address a path a shipped provider parses"),
            _ => None,
        }
    }
}

impl ReadFault {
    pub(crate) fn unsupported(capability: impl Into<String>) -> ReadError {
        Error::new(Self::Unsupported {
            capability: capability.into(),
        })
    }

    /// Classifies a path whose extension no shipped provider parses.
    pub(crate) fn unclaimed_extension(extension: impl Into<String>) -> ReadError {
        Error::new(Self::UnclaimedExtension {
            extension: extension.into(),
        })
    }

    pub(crate) fn invalid(field: &'static str, violation: impl Into<String>) -> ReadError {
        Error::new(Self::Invalid {
            field,
            violation: violation.into(),
        })
    }

    pub(crate) fn not_found(path: impl Into<String>) -> ReadError {
        Error::new(Self::NotFound { path: path.into() })
    }

    /// Classifies a claimed path whose bytes are not valid UTF-8: the index confirmed the
    /// path is real by naming it in its own warnings, so this is `content_unavailable`
    /// rather than `not_found`.
    pub(crate) fn source_unavailable(path: impl Into<String>) -> ReadError {
        Error::new(Self::SourceUnavailable { path: path.into() })
    }

    pub(crate) fn storage(
        path: impl Into<String>,
        operation: &'static str,
        io: &std::io::Error,
    ) -> ReadError {
        Error::new(Self::Storage {
            path: path.into(),
            operation,
            io: io.to_string(),
        })
    }

    /// Keeps an indexing failure's registry identity in one read failure.
    #[must_use]
    pub fn index(source: WorkspaceIndexError) -> ReadError {
        Error::new(Self::Index(source))
    }

    pub(crate) fn history(source: HistoryError) -> ReadError {
        Error::new(Self::History(source))
    }

    /// Classifies a language engine failure, keeping the engine fault's own
    /// wire classification.
    pub(crate) fn engine(source: rift_lsp::session::EngineError) -> ReadError {
        Error::new(Self::Engine(source))
    }

    /// Classifies a Tokio blocking-executor failure.
    pub fn task(operation: &'static str, detail: impl Into<String>) -> ReadError {
        Error::new(Self::Task {
            operation,
            detail: detail.into(),
        })
    }

    /// Classifies validation work that cannot finish within its deadline.
    pub fn unavailable(operation: &'static str, detail: impl Into<String>) -> ReadError {
        Error::new(Self::Unavailable {
            operation,
            detail: detail.into(),
        })
    }

    /// Classifies exhausted wait for bounded blocking capacity.
    #[must_use]
    pub fn capacity_timeout(operation: &'static str, timeout_ms: u64) -> ReadError {
        Error::new(Self::CapacityTimeout {
            operation,
            timeout_ms,
        })
    }
}

/// Opaque read-service failure.
pub type ReadError = Error<ReadFault>;

/// Immutable direct-filesystem workspace read service.
#[derive(Debug)]
pub struct ReadService {
    index: WorkspaceIndex,
    revisions: CapturedRevisions,
    /// The resolved commit this service serves, or null for the current tree.
    revision: Option<RevisionId>,
    /// The accepted `[providers.history]` table: whether symbol history is
    /// served from this snapshot, and how far its walks may reach.
    history: HistoryConfiguration,
    /// The compiled `[source]` policy this snapshot resolved: `Some` for the current
    /// tree, `None` for a revision snapshot, which has no filesystem tree to be
    /// visible in.
    source_policy: Option<Arc<WorkspaceSourcePolicy>>,
}

impl ReadService {
    /// Builds one in-memory snapshot from real workspace files, applying
    /// `visibility`'s `.gitignore` and `[source]` policy on top of the hard
    /// floor. `history` gates and bounds later symbol-history reads served
    /// from this snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when root cannot be indexed within bounds.
    pub fn build(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        history: HistoryConfiguration,
    ) -> Result<Self, ReadError> {
        Self::build_with_languages(
            root,
            limits,
            visibility,
            text_inclusion,
            &LanguageFileSelections::default(),
            BindingPolicy::default(),
            history,
        )
    }

    /// Builds one current-tree snapshot with configured language entries.
    ///
    /// `binding` reaches the workspace index unchanged: it decides whether the
    /// binding provider publishes beside syntax, and under which bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when configuration or root cannot be indexed
    /// within bounds.
    pub fn build_with_languages(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
        binding: BindingPolicy,
        history: HistoryConfiguration,
    ) -> Result<Self, ReadError> {
        let span = tracing::info_span!(
            "index.build",
            component = "index",
            files_count = tracing::field::Empty,
            tree_revision = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let _entered = span.enter();
        let index = WorkspaceIndex::build_with_languages(
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
            binding,
        )
        .map_err(|source| {
            span.record("outcome", "error");
            ReadFault::index(source)
        })?;
        let source_policy = WorkspaceSourcePolicy::build_with_languages(
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
        )
        .map_err(|source| {
            span.record("outcome", "error");
            ReadFault::index(source)
        })?;
        let revisions = captured_revisions(&index);
        span.record("files_count", index.file_count());
        span.record("tree_revision", revisions.wire_tree_revision());
        span.record("outcome", "ok");
        Ok(Self {
            index,
            revisions,
            revision: None,
            history,
            source_policy: Some(Arc::new(source_policy)),
        })
    }

    /// Every file's digest this snapshot indexed, in project-path order.
    ///
    /// A request that captured the tree itself compares its capture with this to name the
    /// files that moved, instead of asking for the whole workspace.
    #[must_use]
    pub fn workspace_digests(&self) -> WorkspaceDigests {
        self.index.digests()
    }

    /// The visibility policy this snapshot reads the filesystem through.
    ///
    /// A revision snapshot answers from version control alone and carries none,
    /// so every capture that touches the tree refuses here first, naming the
    /// operation the caller asked for.
    fn filesystem_policy(
        &self,
        operation: &'static str,
    ) -> Result<&WorkspaceSourcePolicy, ReadError> {
        self.source_policy
            .as_deref()
            .ok_or_else(|| ReadFault::task(operation, "a revision snapshot has no filesystem tree"))
    }

    /// Captures live visible regular-file paths for a workspace change hook.
    ///
    /// Bytes stay absent when configured file or workspace bounds stop content capture.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when this is a revision snapshot or discovery fails.
    pub fn capture_visible_workspace_entries(
        &self,
    ) -> Result<Vec<rift_index::VisibleWorkspaceEntry>, ReadError> {
        self.filesystem_policy("capture visible workspace entries")?
            .visible_entries()
            .map_err(ReadFault::index)
    }

    /// Returns every visible regular file's content digest.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when this is a revision snapshot or capture fails.
    pub fn visible_workspace_digests(&self) -> Result<WorkspaceDigests, ReadError> {
        self.filesystem_policy("capture visible workspace digests")?
            .visible_digests()
            .map_err(ReadFault::index)
    }

    /// The digest of the bytes this snapshot indexed at `path`, or nothing when it indexes
    /// no file there.
    ///
    /// This is what resolves an observation into a change set: a caller hashes the path's
    /// current bytes and compares them with what this snapshot holds.
    #[must_use]
    pub fn file_digest(&self, path: &CoreProjectPath) -> Option<FileDigest> {
        self.index.digest(path)
    }

    /// Builds the next snapshot by reading only the paths `changes` names, sharing every
    /// other file with this one.
    ///
    /// The history configuration and the served revision carry over: an incremental
    /// rebuild answers for the same workspace tree this snapshot did, with the named files
    /// replaced.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when a named path cannot be read or indexed within bounds.
    pub fn rebuilt(&self, changes: &PathChanges) -> Result<Self, ReadError> {
        let span = tracing::info_span!(
            "index.build",
            component = "index",
            changed_count = changes.len(),
            files_count = tracing::field::Empty,
            tree_revision = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let _entered = span.enter();
        let index = self.index.rebuilt(changes).map_err(|source| {
            span.record("outcome", "error");
            ReadFault::index(source)
        })?;
        let revisions = captured_revisions(&index);
        span.record("files_count", index.file_count());
        span.record("tree_revision", revisions.wire_tree_revision());
        span.record("outcome", "ok");
        Ok(Self {
            index,
            revisions,
            revision: self.revision.clone(),
            history: self.history.clone(),
            source_policy: self.source_policy.clone(),
        })
    }

    /// Builds one in-memory snapshot of the workspace at a version-control
    /// revision, read in place from the workspace's repository with no
    /// checkout. The revision tree passes the same `[source]` policy and
    /// bounds as the workspace scan. `history` gates and bounds later
    /// symbol-history reads served from this snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the spelling breaks the advertised
    /// charset, the workspace has no repository, the revision does not
    /// resolve to a commit, or the revision tree cannot be indexed within
    /// bounds.
    pub fn at_revision(
        root: &Path,
        rev: &RevisionId,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        history: HistoryConfiguration,
    ) -> Result<Self, ReadError> {
        Self::at_revision_with_languages(
            root,
            rev,
            limits,
            visibility,
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            history,
        )
    }

    /// Builds one revision snapshot with configured language entries.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when revision or configuration cannot be served.
    pub fn at_revision_with_languages(
        root: &Path,
        rev: &RevisionId,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
        history: HistoryConfiguration,
    ) -> Result<Self, ReadError> {
        if let Some(violation) = rev.violation() {
            return Err(ReadFault::invalid("rev", violation.as_str()));
        }
        let repository = Repository::open(root).map_err(ReadFault::history)?;
        let resolved = repository.resolve(&rev.0).map_err(ReadFault::history)?;
        let index = WorkspaceIndex::at_revision_with_languages(
            &repository,
            &resolved,
            limits,
            visibility,
            text_inclusion,
            languages,
        )
        .map_err(ReadFault::index)?;
        let revisions = captured_revisions(&index);
        Ok(Self {
            index,
            revisions,
            revision: Some(RevisionId(resolved.commit_id())),
            history,
            source_policy: None,
        })
    }

    /// Returns the immutable workspace index this snapshot serves.
    pub(crate) const fn index(&self) -> &WorkspaceIndex {
        &self.index
    }

    /// Returns this snapshot's compiled `[source]` policy: the hard floor, the
    /// `[source]` matcher, and the workspace's `.gitignore` chain. `None` for a
    /// revision snapshot, which has no filesystem tree to be visible in.
    #[must_use]
    pub fn source_policy(&self) -> Option<&WorkspaceSourcePolicy> {
        self.source_policy.as_deref()
    }

    /// Returns this snapshot's compiled `[source]` policy as a shared handle, so a
    /// caller publishing this snapshot beside the workspace index carries the same
    /// value rather than compiling a second one.
    #[must_use]
    pub fn source_policy_handle(&self) -> Option<Arc<WorkspaceSourcePolicy>> {
        self.source_policy.clone()
    }

    /// Returns the warnings every answer from this service carries: one
    /// `stale_index` when the published index lags the captured tree, one
    /// `source_unavailable` for each file the index omitted because its
    /// bytes are not UTF-8, and none of either when nothing applies.
    pub(crate) fn warnings(&self) -> Vec<ReadWarning> {
        let mut warnings = self.revisions.warnings();
        warnings.extend(self.index.warnings().iter().map(wire_index_warning));
        warnings
    }

    /// Returns the resolved commit this service serves, or none for the
    /// current tree.
    pub(crate) const fn revision(&self) -> Option<&RevisionId> {
        self.revision.as_ref()
    }

    /// Returns exact visible source identity captured by this service.
    #[must_use]
    pub const fn workspace_fingerprint(&self) -> &WorkspaceFingerprint {
        self.index.fingerprint()
    }

    /// Returns the tree revision this service captured, in its
    /// eight-hex-character wire form. A lexical population stamps this exact
    /// string, and a search request compares its query-time lexical
    /// revision against it, so the two never drift apart.
    #[must_use]
    pub fn tree_revision(&self) -> &str {
        self.revisions.wire_tree_revision()
    }

    /// Derives this snapshot's lexical search units: one per indexed
    /// symbol and baseline text file, chunked where text exceeds
    /// `[search.text].max_chunk`.
    #[must_use]
    pub fn lexical_units(&self) -> Vec<rift_index::LexicalUnit> {
        self.index.lexical_units()
    }

    /// Returns each baseline text file split into more than one lexical
    /// chunk, paired with its chunk count, so a caller can warn about the
    /// split instead of it passing silently.
    #[must_use]
    pub fn chunked_text_files(&self) -> Vec<(CoreProjectPath, usize)> {
        self.index.chunked_text_files()
    }

    /// Reads syntax nodes covering one UTF-8 byte position. The tree
    /// the nodes come from is the one this service holds; `params.rev` was
    /// already honored by building the service at that revision.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for invalid paths, missing files, or a `position` at or past the
    /// file's byte length.
    pub fn nodes(&self, params: NodesParams) -> Result<NodesResult, ReadError> {
        validate_common(params.rev.is_some())?;
        let path = CoreProjectPath::new(params.path.0).map_err(|error| {
            ReadFault::invalid("path", rift_core::fault_label(&error.fault().violation()))
        })?;
        let file = self
            .index
            .file(&path)
            .ok_or_else(|| self.missing_file_fault(&path))?;
        let source_len = file.source().len() as u64;
        if params.position >= source_len {
            return Err(ReadFault::invalid(
                "position",
                format!(
                    "{} is at or past the file's byte length {source_len}",
                    params.position
                ),
            ));
        }
        let matched = file.syntax().nodes_at(params.position);
        let nodes = matched.iter().map(|node| wire_node(file, node)).collect();
        let source = matched
            .iter()
            .map(|node| excerpt(file, node.range))
            .collect();
        Ok(NodesResult {
            nodes,
            source,
            warnings: self.warnings(),
        })
    }

    /// The failure for a path the syntax index does not hold: `content_unavailable` when
    /// this snapshot's own warnings name `path` as holding invalid UTF-8 - the file exists
    /// but could not be read - the capability a syntax read lacks when this snapshot can
    /// confirm the path is real, and `not_found` for everything else, including a claimed
    /// path the index simply has not read.
    fn missing_file_fault(&self, path: &CoreProjectPath) -> ReadError {
        if self.index_names_invalid_utf8(path) {
            return ReadFault::source_unavailable(path.as_str());
        }
        match self.unserved_syntax(path) {
            Ok(Some(UnservedSyntax::Configurable(capability))) => {
                ReadFault::unsupported(capability)
            }
            Ok(Some(UnservedSyntax::Unclaimed(extension))) => {
                ReadFault::unclaimed_extension(extension)
            }
            Ok(None) => ReadFault::not_found(path.as_str()),
            Err(storage_fault) => storage_fault,
        }
    }

    /// Whether this snapshot's own warnings name `path` as holding bytes that are not
    /// UTF-8: the one condition the index confirms without the path being indexed.
    fn index_names_invalid_utf8(&self, path: &CoreProjectPath) -> bool {
        self.index
            .warnings()
            .iter()
            .any(|warning| warning.path() == path)
    }

    /// The syntax capability `path` lacks, reported only when this snapshot can
    /// confirm the path is real.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when two language entries match `path`, or when the
    /// filesystem cannot answer whether it exists.
    fn unserved_syntax(&self, path: &CoreProjectPath) -> Result<Option<UnservedSyntax>, ReadError> {
        let Some(reason) = self.unserved_syntax_reason(path)? else {
            return Ok(None);
        };
        Ok(self.path_is_real(path)?.then_some(reason))
    }

    /// Why a syntax read cannot serve `path`, or nothing when an enabled language
    /// entry with a shipped provider claims it.
    fn unserved_syntax_reason(
        &self,
        path: &CoreProjectPath,
    ) -> Result<Option<UnservedSyntax>, ReadError> {
        let claim = self
            .index
            .language_policy()
            .language_for_path(Path::new(path.as_str()))
            .map_err(ReadFault::index)?;
        Ok(match claim {
            Some(language) if language.enabled() && language.has_syntax() => None,
            Some(language) => Some(UnservedSyntax::Configurable(format!(
                "{} files",
                language.identity()
            ))),
            None => Some(UnservedSyntax::Unclaimed(extension_capability(path))),
        })
    }

    /// The exact language identity an engine is asked under for `path`: the
    /// indexed file's own language, or the effective entry claiming the path when
    /// no shipped provider parses it. An unshipped language reaches an engine this
    /// way, because its entry selects a process without contributing syntax facts.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when two language entries match `path`.
    pub fn engine_language_segment(
        &self,
        path: &CoreProjectPath,
    ) -> Result<Option<String>, ReadError> {
        if let Some(file) = self.index.file(path) {
            return Ok(Some(file.syntax().language().identity_segment()));
        }
        Ok(self
            .index
            .language_policy()
            .language_for_path(Path::new(path.as_str()))
            .map_err(ReadFault::index)?
            .filter(|language| language.enabled())
            .map(|language| language.identity().to_owned()))
    }

    /// Whether this snapshot can confirm `path` names a real visible file. On the
    /// current tree the `[source]` policy has to make `path` visible and the
    /// filesystem has to hold it; a revision snapshot carries no filesystem policy,
    /// so the tree it was built from is the answer.
    fn path_is_real(&self, path: &CoreProjectPath) -> Result<bool, ReadError> {
        let Some(policy) = self.source_policy.as_deref() else {
            return Ok(true);
        };
        let absolute = self.index.root().join(path.as_str());
        if !policy.visible(&absolute) {
            return Ok(false);
        }
        absolute
            .try_exists()
            .map_err(|error| ReadFault::storage(path.as_str(), "stat", &error))
    }

    /// Finds declarations by name, with each hit's version-control timeline
    /// when the request asks for history.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for symbol history the workspace's version control cannot
    /// serve.
    pub fn get_symbol(&self, params: &GetSymbolParams) -> Result<GetSymbolResult, ReadError> {
        validate_common(params.rev.is_some())?;
        let limit = accepted_limit(params.limit)?;
        // The whole ranked match set is collected up to the index's own `results_max`
        // bound, so `pagination.total_pages` counts the full result set the pages divide.
        let mut matches = self
            .index
            .symbols(&params.name, self.index.results_max())
            .map_err(ReadFault::index)?;
        if let Some(language) = &params.language {
            matches.retain(|matched| language_selects(language, matched.file.syntax().language()));
        }
        let (window, pagination) = page(matches, params.page_index, limit);
        let mut timelines = if params.include_history {
            Some(self.symbol_timelines()?)
        } else {
            None
        };
        let mut hits = Vec::with_capacity(window.len());
        for matched in window {
            let history = match timelines.as_mut() {
                Some(timelines) => Some(
                    timelines
                        .timeline(language_provider(matched.file.syntax().language()), matched)?,
                ),
                None => None,
            };
            hits.push(GetSymbolHit {
                symbol: wire_symbol(&self.index, matched)?,
                span: source_span(matched.file.path(), matched.symbol.range),
                node: params.include_body.then(|| symbol_node(matched)),
                source: params
                    .include_body
                    .then(|| excerpt(matched.file, matched.symbol.range)),
                history,
            });
        }
        Ok(GetSymbolResult {
            hits,
            pagination,
            warnings: self.warnings(),
        })
    }

    /// Opens this snapshot's timeline composition, walking from the served
    /// commit for a revision read and from `HEAD` for a current-tree read.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when `[providers.history]` is disabled or the
    /// workspace's version control cannot serve a walk start.
    fn symbol_timelines(&self) -> Result<SymbolTimelines, ReadError> {
        SymbolTimelines::open(self.index.root(), self.revision.as_ref(), &self.history)
    }
}

/// Accepts a caller-supplied result limit: positive, and inside this
/// platform's addressable range.
pub(crate) fn accepted_limit(requested: u64) -> Result<usize, ReadError> {
    if requested == 0 {
        return Err(ReadFault::invalid("limit", "zero"));
    }
    usize::try_from(requested)
        .map_err(|_| ReadFault::invalid("limit", format!("{requested} exceeds this platform")))
}

/// The one checkpoint every read call passes before its own validation. `projection` and
/// every `scope` but `project` left the wire, so `rev` is the only field this still takes;
/// a caller-supplied `rev` needs no rule beyond what [`ReadService::at_revision`] already
/// enforces when it resolves that revision.
#[expect(
    clippy::unnecessary_wraps,
    reason = "kept as Result for symmetry with every other read validation, which every call \
              site propagates with `?`"
)]
pub(crate) fn validate_common(_rev: bool) -> Result<(), ReadError> {
    Ok(())
}

/// Cuts one page out of a fully collected result set and states where the page sits.
///
/// Work is bounded upstream: every caller collects at most the index's `results_max`
/// results before paging. `limit` is positive - `accepted_limit` refuses zero - so
/// `total_pages` is `results.len().div_ceil(limit)`, zero for an empty set. A
/// `page_index` past the last page yields an empty page carrying the requested index
/// and the true page count.
pub(crate) fn page<T>(results: Vec<T>, page_index: u64, limit: usize) -> (Vec<T>, Pagination) {
    assert!(
        limit > 0,
        "page limit must be positive after acceptance: limit={limit}"
    );
    let total = results.len();
    let total_pages = u64::try_from(total.div_ceil(limit)).unwrap_or(u64::MAX);
    let pagination = Pagination {
        page_index,
        total_pages,
    };
    let start = usize::try_from(page_index)
        .ok()
        .and_then(|index| index.checked_mul(limit));
    let window = match start {
        Some(start) if start < total => results.into_iter().skip(start).take(limit).collect(),
        _ => Vec::new(),
    };
    (window, pagination)
}

fn wire_node(file: &IndexedFile, node: &SyntaxNode) -> Node {
    let language = file.syntax().language();
    Node {
        id: node_id(file, node),
        symbol: symbol_for_range(file, node.range).map(|symbol| symbol_id(file, symbol)),
        unit: file_id(file.path()),
        language: language.clone(),
        kind: wire_kind(language, &node.kind),
        facets: language_provider(language).node_facets(&node.kind),
        range: text_range(node.range),
        regions: Vec::new(),
        parent: None,
        extensions: Extensions(BTreeMap::new()),
    }
}

fn symbol_node(matched: SymbolMatch<'_>) -> Node {
    let node = matched
        .file
        .syntax()
        .nodes()
        .iter()
        .find(|node| node.range == matched.symbol.range);
    node.map_or_else(
        || {
            let language = matched.file.syntax().language();
            Node {
                id: NodeId(node_address(matched.file, matched.symbol.range)),
                symbol: Some(symbol_id(matched.file, matched.symbol)),
                unit: file_id(matched.file.path()),
                language: language.clone(),
                kind: wire_kind(language, matched.symbol.kind),
                facets: vec![NodeFacet::Declaration, NodeFacet::Definition],
                range: text_range(matched.symbol.range),
                regions: Vec::new(),
                parent: None,
                extensions: Extensions(BTreeMap::new()),
            }
        },
        |node| wire_node(matched.file, node),
    )
}

pub(crate) fn wire_symbol(
    index: &WorkspaceIndex,
    matched: SymbolMatch<'_>,
) -> Result<Symbol, ReadError> {
    let readable = index.assembled_symbol(matched).map_err(ReadFault::index)?;
    Ok(assembled_wire_symbol(matched, &readable))
}

fn assembled_wire_symbol(matched: SymbolMatch<'_>, readable: &ReadableSymbol) -> Symbol {
    let assembled = readable.assembled();
    let facts = readable.facts();
    let mut extension_values = BTreeMap::new();
    for (_, extensions) in assembled.namespaced() {
        for (key, value) in &extensions.0 {
            extension_values
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
    Symbol {
        index_revision: assembled.index_revision().get(),
        id: readable
            .identity()
            .map(|identity| SymbolId(identity.as_str().to_owned())),
        resolution: wire_symbol_resolution(assembled.resolution()),
        contributions: assembled
            .contributions()
            .iter()
            .map(wire_contribution_key)
            .collect(),
        language: facts.language().clone(),
        name: facts.name().to_owned(),
        kind: facts.kind().clone(),
        facets: facts.symbol_facets().to_vec(),
        origin: SymbolOrigin {
            location: assembled.origin().location().cloned(),
            source_kind: assembled.origin().source_kind(),
            unit: Some(source_unit_id(matched.file.path())),
        },
        container: assembled
            .container()
            .map(|container| SymbolId(container.as_str().to_owned())),
        modifiers: facts.modifier_words().to_vec(),
        visibility: facts.visibility_spelling().map(str::to_owned),
        types: facts.type_bindings().to_vec(),
        signatures: facts.signatures_slice().to_vec(),
        documentation: facts.documentation_blocks().to_vec(),
        extensions: Extensions(extension_values),
        disagreements: assembled
            .disagreements()
            .iter()
            .map(|disagreement| SymbolDisagreement {
                contribution: wire_contribution_key(disagreement.contribution()),
                field: wire_presentation_field(disagreement.field()),
            })
            .collect(),
        document_local: facts.is_document_local(),
    }
}

fn wire_contribution_key(key: &rift_core::ContributionKey) -> ContributionKey {
    ContributionKey {
        provider: key.reference().provider().as_str().to_owned(),
        symbol: key.reference().symbol().as_str().to_owned(),
        publication: key.publication().get(),
    }
}

fn wire_symbol_resolution(resolution: rift_core::SymbolResolution) -> SymbolResolution {
    match resolution {
        rift_core::SymbolResolution::Established => SymbolResolution::Established,
        rift_core::SymbolResolution::Unresolved => SymbolResolution::Unresolved,
        rift_core::SymbolResolution::Conflicting => SymbolResolution::Conflicting,
    }
}

fn wire_presentation_field(field: rift_provider::PresentationField) -> SymbolPresentationField {
    match field {
        rift_provider::PresentationField::Language => SymbolPresentationField::Language,
        rift_provider::PresentationField::Name => SymbolPresentationField::Name,
        rift_provider::PresentationField::QualifiedName => SymbolPresentationField::QualifiedName,
        rift_provider::PresentationField::Kind => SymbolPresentationField::Kind,
        rift_provider::PresentationField::Container => SymbolPresentationField::Container,
        rift_provider::PresentationField::Visibility => SymbolPresentationField::Visibility,
        rift_provider::PresentationField::DocumentLocal => SymbolPresentationField::DocumentLocal,
        rift_provider::PresentationField::Origin => SymbolPresentationField::Origin,
    }
}

pub(crate) fn excerpt(file: &IndexedFile, range: ByteRange) -> SourceExcerpt {
    let start = usize::try_from(range.start)
        .unwrap_or(file.source().len())
        .min(file.source().len());
    let end = usize::try_from(range.end)
        .unwrap_or(file.source().len())
        .min(file.source().len());
    let text = file.source().get(start..end).unwrap_or_default().to_owned();
    SourceExcerpt {
        span: source_span(file.path(), range),
        text,
    }
}

pub(crate) fn source_span(path: &CoreProjectPath, range: ByteRange) -> SourceUnitSpan {
    SourceUnitSpan {
        unit: source_unit_id(path),
        range: text_range(range),
    }
}

fn text_range(range: ByteRange) -> TextRange {
    TextRange {
        start: range.start,
        end: range.end,
    }
}

/// Composes the wire kind for one grammar fact: the language name, a dot,
/// the provider's kind word, as in `rust.function_item`.
fn wire_kind(language: &Language, kind: &str) -> ExactKind {
    ExactKind(format!("{}.{kind}", language.name))
}

/// The registered provider filing facts under `language`.
///
/// Panics when no registered provider serves it: the index only produces
/// documents through registered providers, so an unserved language here is a
/// programmer invariant break, not a reachable operating state.
pub(crate) fn language_provider(language: &Language) -> &'static dyn SyntaxProvider {
    registry::provider_for_language(language).unwrap_or_else(|| {
        panic!(
            "an indexed document's language must have a registered syntax provider: language={}",
            language.identity_segment()
        )
    })
}

/// A caller's `language` filter selects a document language when the names
/// match and, where the filter states a dialect, the dialects match too. A
/// filter without a dialect selects every dialect of its name.
fn language_selects(filter: &Language, candidate: &Language) -> bool {
    filter.name == candidate.name
        && (filter.dialect.is_none() || filter.dialect == candidate.dialect)
}

/// Why a syntax read cannot serve one path.
#[derive(Debug)]
enum UnservedSyntax {
    /// A language entry claims the path, and this workspace either turned that
    /// entry off or ships no grammar for it. Configuration can serve the path.
    Configurable(String),
    /// No language entry claims the path, so no shipped grammar parses it.
    Unclaimed(String),
}

/// The capability text for a path no language entry claims: the extension
/// itself, or a statement that the path carries none.
fn extension_capability(path: &CoreProjectPath) -> String {
    match Path::new(path.as_str()).extension().and_then(OsStr::to_str) {
        Some(extension) => format!("{extension} files"),
        None => "files with no extension".to_owned(),
    }
}

pub(crate) fn file_id(path: &CoreProjectPath) -> FileId {
    FileId(format!(
        "rift://file/{}",
        rift_core::encode_path(path.as_str())
    ))
}

/// Projects one index-build warning onto its wire form.
fn wire_index_warning(warning: &WorkspaceIndexWarning) -> ReadWarning {
    let (path, detail) = match warning {
        WorkspaceIndexWarning::InvalidUtf8Source(path) => (
            path,
            format!(
                "{path}'s bytes are not valid UTF-8, so the file is absent from the index",
                path = path.as_str(),
            ),
        ),
        WorkspaceIndexWarning::BinarySource(path) => (
            path,
            format!(
                "{path} contains a NUL byte, so the file is absent from the index",
                path = path.as_str(),
            ),
        ),
        WorkspaceIndexWarning::FileTooLarge(path) => (
            path,
            format!(
                "{path} exceeds the file byte limit, so the file is absent from the index",
                path = path.as_str(),
            ),
        ),
    };

    ReadWarning::SourceUnavailable {
        unit: file_id(path),
        detail,
    }
}

/// Mints the project resolver's source-unit identity: the resolver name, then the
/// project-relative path as the resolver's own canonical unit key.
pub(crate) fn source_unit_id(path: &CoreProjectPath) -> SourceUnitId {
    SourceUnitId(format!(
        "rift://source/project/{}",
        rift_core::encode_path(path.as_str())
    ))
}

/// Project-relative path, as the wire model carries it.
pub(crate) fn project_path(path: &CoreProjectPath) -> ProjectPath {
    ProjectPath(path.as_str().to_owned())
}

pub(crate) fn symbol_id(file: &IndexedFile, symbol: &SyntaxSymbol) -> SymbolId {
    SymbolId(rift_core::symbol_identity(
        &file.syntax().language().identity_segment(),
        file.path().as_str(),
        &symbol.qualified_name,
    ))
}

fn node_id(file: &IndexedFile, node: &SyntaxNode) -> NodeId {
    NodeId(node_address(file, node.range))
}

fn node_address(file: &IndexedFile, range: ByteRange) -> String {
    format!(
        "rift://node/{}/{}@{}-{}#{}",
        file.syntax().language().identity_segment(),
        rift_core::encode_path(file.path().as_str()),
        range.start,
        range.end,
        node_witness(file.source(), range)
    )
}

/// Tree revisions captured when one read service is built, at full SHA-256
/// length: the `stale_index` comparison runs over the full digests, and only
/// the truncated wire form reaches a warning.
#[derive(Clone, Debug)]
pub(crate) struct CapturedRevisions {
    /// Full digest of the targeted tree when the read began.
    tree_revision: String,
    /// Full digest of the tree the published index covers.
    index_tree_revision: String,
}

impl CapturedRevisions {
    /// Warnings for an answer served from these revisions: one `stale_index`
    /// when the published index lags the tree the read captured, none when
    /// the two digests match.
    pub(crate) fn warnings(&self) -> Vec<ReadWarning> {
        if self.index_tree_revision == self.tree_revision {
            return Vec::new();
        }
        let index_tree_revision = wire_digest(&self.index_tree_revision);
        let captured_tree_revision = wire_digest(&self.tree_revision);
        let detail = format!(
            "the answer was computed from an index at tree revision {} that lags the \
             captured tree revision {}; resend the request after the server publishes a \
             fresh snapshot",
            index_tree_revision.0, captured_tree_revision.0,
        );
        vec![ReadWarning::StaleIndex {
            index_tree_revision,
            captured_tree_revision,
            detail,
        }]
    }

    /// The captured tree revision truncated to its wire form: the first
    /// `DIGEST_WIRE_CHARS` lowercase hex characters.
    pub(crate) fn wire_tree_revision(&self) -> &str {
        &self.tree_revision[..DIGEST_WIRE_CHARS]
    }
}

/// Separates one path from its content digest in tree-revision material.
const TREE_REVISION_PATH_SEPARATOR: u8 = 0;
/// Separates adjacent files in tree-revision material.
const TREE_REVISION_FILE_SEPARATOR: u8 = 0xff;

/// The revisions one read service captures at build time. The captured tree
/// and the indexed tree both derive from the one index the service holds, so
/// a service built here observes them equal; `CapturedRevisions::warnings`
/// guards the comparison all the same, so an index resolved apart from its
/// capture cannot lag silently.
fn captured_revisions(index: &WorkspaceIndex) -> CapturedRevisions {
    let digest = workspace_digest(index);
    CapturedRevisions {
        tree_revision: digest.clone(),
        index_tree_revision: digest,
    }
}

/// Folds the index's own files, in project-path order, absorbing each file's content
/// digest rather than its bytes. The index already carries that digest from the bytes it
/// read, so a rebuild that replaced one file pays one hash update per file instead of
/// rehashing every source it shared with the previous publication.
fn workspace_digest(index: &WorkspaceIndex) -> String {
    let mut hasher = Sha256::new();
    for file in index.files() {
        hasher.update(file.path().as_str().as_bytes());
        hasher.update([TREE_REVISION_PATH_SEPARATOR]);
        hasher.update(file.digest().as_bytes());
        hasher.update([TREE_REVISION_FILE_SEPARATOR]);
    }
    format!("{:x}", hasher.finalize())
}

/// The witness a node address carries: the first eight lowercase hex characters of the
/// SHA-256 of the node's source bytes. Recomputing it is how resolution proves the bytes
/// behind an address have not drifted.
///
/// `range` must already land inside `source`: minting hashes a real indexed node's own
/// range, and [`resolve_node_range`] proves a caller-supplied range lands before calling
/// this. Neither caller clamps, so this does not either.
///
/// # Panics
///
/// Panics when `range` does not land inside `source` - a programmer error, since every
/// caller proves the range first.
pub(crate) fn node_witness(source: &str, range: ByteRange) -> String {
    let start = usize::try_from(range.start).expect("node range start fits this platform's usize");
    let end = usize::try_from(range.end).expect("node range end fits this platform's usize");
    let bytes = source
        .get(start..end)
        .expect("node range lands inside the source it was minted or resolved against");
    digest_hex8(bytes)
}

/// One witnessed node range, resolved and verified against an indexed file: whether the
/// bytes it now hashes to still match the address's own witness.
#[derive(Debug)]
pub(crate) enum NodeRangeResolution {
    /// The range lands on an indexed node and its bytes still hash to the given witness.
    Verified,
    /// The range lands on an indexed node, but its bytes hash to a different witness.
    WitnessChanged {
        /// The witness recomputed from the file's current bytes.
        observed: String,
    },
}

/// Resolves one witnessed node range against `file`: every witnessed node read and write
/// proves its range through this call, and nothing else compares or clamps a node witness.
///
/// The range must land on an indexed node's exact bytes - in bounds, on a UTF-8 character
/// boundary at both ends, and equal to one of `file.syntax().nodes()`'s own ranges - before
/// its witness is even computed. A range that does not land refuses here, naming the range
/// rather than silently clamping it to whatever the file can still offer. A range that lands
/// but hashes to a different witness is reported as [`NodeRangeResolution::WitnessChanged`]
/// instead, so the caller can build its own `source_unchanged` precondition.
///
/// # Errors
///
/// Returns [`ReadError`] naming `range outside the addressed file` when the range does not
/// land on an indexed node.
pub(crate) fn resolve_node_range(
    file: &IndexedFile,
    range: ByteRange,
    witness: &str,
) -> Result<NodeRangeResolution, ReadError> {
    if !range_names_an_indexed_node(file, range) {
        return Err(ReadFault::invalid(
            "span",
            "range outside the addressed file",
        ));
    }
    let observed = node_witness(file.source(), range);
    Ok(if observed == witness {
        NodeRangeResolution::Verified
    } else {
        NodeRangeResolution::WitnessChanged { observed }
    })
}

/// Whether `range` lands exactly on one of `file`'s indexed syntax nodes: in bounds, on a
/// UTF-8 character boundary at both ends, and equal to a real node's own range.
fn range_names_an_indexed_node(file: &IndexedFile, range: ByteRange) -> bool {
    let source = file.source();
    let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end)) else {
        return false;
    };
    start <= end
        && end <= source.len()
        && source.is_char_boundary(start)
        && source.is_char_boundary(end)
        && file.syntax().nodes().iter().any(|node| node.range == range)
}

/// First `DIGEST_WIRE_CHARS` lowercase hex characters of the SHA-256 of `source` - the sole
/// wire constructor for a witness or a `Digest`. A 64-character digest reaching the wire is a
/// defect this stays the single choke point against.
pub(crate) fn digest_hex8(source: &str) -> String {
    digest_wire_hex(&Sha256::digest(source.as_bytes()))
}

/// Truncates an already-hashed full-length hex digest to its wire form. `full` keeps
/// collision resistance for internal identity computation; only the truncated form crosses
/// the wire boundary.
fn wire_digest(full: &str) -> Digest {
    Digest(full[..DIGEST_WIRE_CHARS].to_owned())
}

/// Renders one already-computed SHA-256 digest in the `DIGEST_WIRE_CHARS` wire form, the
/// truncation behind [`digest_hex8`] and the minted `ChangeId`.
pub(crate) fn digest_wire_hex(digest: &sha2::digest::Output<Sha256>) -> String {
    format!("{digest:x}")[..DIGEST_WIRE_CHARS].to_owned()
}

/// Finds the symbol a witnessed syntax node belongs to.
///
/// A node's range matches a symbol's declaration range (the whole
/// declaration, including attached docs and attributes) for most nodes, but
/// the item node itself only spans its own bytes, so it matches on
/// `item_range` instead.
pub(crate) fn symbol_for_range(file: &IndexedFile, range: ByteRange) -> Option<&SyntaxSymbol> {
    file.syntax()
        .symbols()
        .iter()
        .find(|symbol| symbol.range == range || symbol.item_range == range)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use rift_core::{LanguageFileSelections, SourceVisibility};
    use rift_protocol::configuration::{LanguageConfiguration, WorkspaceConfiguration};
    use rift_protocol::read::{
        GetSymbolParams, Language, NodeFacet, NodesParams, NodesResult, ProjectPath, ReadWarning,
        RevisionId,
    };
    use rift_syntax::ByteRange;
    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        BindingPolicy, HistoryConfiguration, ReadFault, ReadService, WorkspaceIndexLimits, file_id,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    /// A read snapshot over `root` under `limits` and `languages`, built through the
    /// same entry point the server itself uses.
    fn reads_with(
        root: &std::path::Path,
        limits: WorkspaceIndexLimits,
        text_inclusion: &rift_core::TextFileInclusion,
        languages: &LanguageFileSelections,
    ) -> Result<ReadService, super::ReadError> {
        ReadService::build_with_languages(
            root,
            limits,
            &SourceVisibility::default(),
            text_inclusion,
            languages,
            BindingPolicy::default(),
            HistoryConfiguration::default(),
        )
    }

    /// One workspace whose `rust` entry is turned off: `lib.rs` stays a real visible
    /// file that no shipped grammar reaches.
    fn disabled_rust_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "rust".to_owned(),
            LanguageConfiguration {
                enabled: false,
                ..LanguageConfiguration::default()
            },
        );
        let languages = LanguageFileSelections::from(&configuration);
        let limits = WorkspaceIndexLimits::default();
        let text_inclusion = rift_core::TextFileInclusion::default();
        let service = reads_with(directory.path(), limits, &text_inclusion, &languages)?;
        Ok((directory, service))
    }

    #[test]
    fn nodes_on_a_path_a_disabled_language_claims_name_the_configurable_capability() -> TestResult {
        let (_directory, service) = disabled_rust_fixture()?;

        let error = service
            .nodes(NodesParams {
                path: ProjectPath("lib.rs".to_owned()),
                position: 0,
                rev: None,
            })
            .expect_err("a language entry that is turned off serves no syntax");

        let fault = error.fault();
        assert!(
            matches!(fault, ReadFault::Unsupported { capability } if capability == "rust files"),
            "an entry the workspace can turn back on refuses as a capability, not as an \
             unclaimed extension: {fault:?}"
        );
        Ok(())
    }

    #[test]
    fn a_gitignore_chain_past_the_file_bound_refuses_the_snapshot() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".gitignore"), "build\n")?;
        fs::create_dir(directory.path().join("nested"))?;
        fs::write(directory.path().join("nested/.gitignore"), "out\n")?;
        let limits = WorkspaceIndexLimits::new(1, 1_024, 1_048_576, 16, 64)?;
        // No text pattern selects the ignore files themselves, so the index reads none of
        // them and the file bound is spent on the chain the source policy compiles.
        let text_inclusion = rift_core::TextFileInclusion::new(Vec::new(), 1_024);
        let languages = LanguageFileSelections::default();

        let error = reads_with(directory.path(), limits, &text_inclusion, &languages)
            .expect_err("an ignore chain past the file bound cannot be compiled");

        let fault = error.fault();
        assert!(
            matches!(fault, ReadFault::Index(_)),
            "unexpected fault {fault:?}"
        );
        Ok(())
    }

    fn fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub struct Beacon;\nimpl Beacon { pub fn signal(&self) {} }\n",
        )?;
        fs::write(directory.path().join("README.md"), "Beacon docs")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    const DOCUMENTED_SOURCE: &str = "/// A beacon.\n#[derive(Debug)]\npub struct Beacon;\n";

    fn documented_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), DOCUMENTED_SOURCE)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    /// Exercises every Rust declaration kind, a private and a restricted
    /// visibility, and a comment plus an expression statement.
    const RICH_SOURCE: &str = r#"pub enum Level { Low, High }

pub trait Speaks {
    fn say(&self);
}

pub type Alias = u32;

pub const MAX: u32 = 10;

pub static NAME: &str = "beacon";

pub mod inner {
    pub fn nested() {}
}

macro_rules! noop {
    () => {};
}

struct Hidden;

pub(crate) fn scoped() {}

pub fn compute() -> i32 {
    // lookout marker
    let total = 1 + 2;
    total;
    0
}
"#;

    fn rich_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), RICH_SOURCE)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    fn any_node_has_facet(result: &NodesResult, facet: &str) -> TestResult<bool> {
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        Ok(nodes.iter().any(|node| {
            node["facets"]
                .as_array()
                .is_some_and(|facets| facets.contains(&json!(facet)))
        }))
    }

    #[test]
    fn nodes_return_typed_rust_facts_from_real_file() -> TestResult {
        let (_directory, service) = fixture()?;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 5,
            rev: None,
        })?;
        let value = serde_json::to_value(result)?;

        assert!(
            value["nodes"]
                .as_array()
                .is_some_and(|nodes| !nodes.is_empty())
        );
        assert_eq!(value["nodes"][0]["language"]["name"], "rust");
        assert!(
            value["nodes"][0]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("rift://node/rust/"))
        );
        assert!(
            value.get("warnings").is_none(),
            "a live nodes result must omit warnings when there is nothing to warn about"
        );
        Ok(())
    }

    #[test]
    fn nodes_at_the_source_length_and_beyond_refuse_naming_the_position() -> TestResult {
        let (directory, service) = fixture()?;
        let source = fs::read_to_string(directory.path().join("src/lib.rs"))?;
        let length = source.len() as u64;

        for position in [length, length + 1] {
            let error = service
                .nodes(NodesParams {
                    path: ProjectPath("src/lib.rs".to_owned()),
                    position,
                    rev: None,
                })
                .expect_err("a position at or past the file's byte length must refuse");
            let ReadFault::Invalid { field, .. } = error.fault() else {
                panic!("expected Invalid, got {:?}", error.fault());
            };
            assert_eq!(*field, "position");
        }
        Ok(())
    }

    #[test]
    fn nodes_at_the_last_in_bounds_byte_position_still_answers() -> TestResult {
        let (directory, service) = fixture()?;
        let source = fs::read_to_string(directory.path().join("src/lib.rs"))?;
        let length = source.len() as u64;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: length - 1,
            rev: None,
        })?;
        assert!(
            !result.nodes.is_empty(),
            "the last in-bounds byte position must still answer"
        );
        Ok(())
    }

    #[test]
    fn nodes_at_a_byte_inside_a_multi_byte_character_returns_its_enclosing_nodes() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        let source = "pub fn beacon() -> &'static str {\n    \"café\"\n}\n";
        fs::write(directory.path().join("src/lib.rs"), source)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let cafe_index = source.find("café").ok_or("fixture must hold café")?;
        // 'é' starts right after "caf" and is two bytes wide; one byte past its start sits
        // inside its encoding, off any character boundary.
        let mid_character_position = cafe_index + "caf".len() + 1;
        assert!(
            !source.is_char_boundary(mid_character_position),
            "the chosen position must sit inside the multi-byte character"
        );
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: u64::try_from(mid_character_position)
                .expect("the fixture offset fits in u64"),
            rev: None,
        })?;
        assert!(
            !result.nodes.is_empty(),
            "a position mid-character still returns its enclosing nodes"
        );
        Ok(())
    }

    #[test]
    fn nodes_source_carries_one_excerpt_per_node_in_order_spanning_its_own_range() -> TestResult {
        let (_directory, service) = fixture()?;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 5,
            rev: None,
        })?;
        assert!(
            result.nodes.len() > 1,
            "the fixture position must be covered by more than one node"
        );
        assert_eq!(
            result.nodes.len(),
            result.source.len(),
            "one excerpt must ride per listed node"
        );
        for (node, source) in result.nodes.iter().zip(result.source.iter()) {
            assert_eq!(
                source.span.range, node.range,
                "each excerpt must span its own node's range, not the requested position"
            );
        }
        Ok(())
    }

    #[test]
    fn resolve_node_range_verifies_a_real_node_and_reports_a_changed_witness() -> TestResult {
        let (_directory, service) = fixture()?;
        let path = rift_core::ProjectPath::new("src/lib.rs")?;
        let file = service.index().file(&path).ok_or("fixture file indexed")?;
        // A real address, read back through `nodes` the way a caller would, so its witness
        // is the one production minting already computed rather than a second call here.
        let listing = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 0,
            rev: None,
        })?;
        let listed = &listing.nodes[0];
        let witness = listed
            .id
            .0
            .rsplit_once('#')
            .map(|(_, witness)| witness.to_owned())
            .ok_or("a listed node id must carry a witness")?;
        let range = ByteRange {
            start: listed.range.start,
            end: listed.range.end,
        };

        let verified = super::resolve_node_range(file, range, &witness)?;
        assert!(matches!(verified, super::NodeRangeResolution::Verified));

        let changed = super::resolve_node_range(file, range, "00000000")?;
        let super::NodeRangeResolution::WitnessChanged { observed } = changed else {
            panic!("a wrong witness must report the change, not refuse the range");
        };
        assert_eq!(observed, witness);
        Ok(())
    }

    #[test]
    fn resolve_node_range_refuses_a_range_past_the_source_length() -> TestResult {
        let (_directory, service) = fixture()?;
        let path = rift_core::ProjectPath::new("src/lib.rs")?;
        let file = service.index().file(&path).ok_or("fixture file indexed")?;
        let source_len = file.source().len() as u64;
        let range = ByteRange {
            start: 0,
            end: source_len + 10,
        };
        let error = super::resolve_node_range(file, range, "00000000")
            .expect_err("a range past the source length must refuse");
        let ReadFault::Invalid { field, violation } = error.fault() else {
            panic!("expected Invalid, got {:?}", error.fault());
        };
        assert_eq!(*field, "span");
        assert!(violation.contains("outside the addressed file"));
        Ok(())
    }

    #[test]
    fn resolve_node_range_refuses_an_in_bounds_range_naming_no_syntax_node() -> TestResult {
        let (_directory, service) = fixture()?;
        let path = rift_core::ProjectPath::new("src/lib.rs")?;
        let file = service.index().file(&path).ok_or("fixture file indexed")?;
        // One byte into the file: inside the leading `pub` keyword, but not equal to any
        // node's own range.
        let range = ByteRange { start: 1, end: 2 };
        let witness = super::digest_hex8(&file.source()[1..2]);
        let error = super::resolve_node_range(file, range, &witness)
            .expect_err("a range naming no syntax node must refuse");
        let ReadFault::Invalid { field, violation } = error.fault() else {
            panic!("expected Invalid, got {:?}", error.fault());
        };
        assert_eq!(*field, "span");
        assert!(
            violation.contains("outside the addressed file"),
            "the refusal must name the range, not a witness mismatch: {violation}"
        );
        Ok(())
    }

    #[test]
    fn get_symbol_span_is_set_whether_or_not_the_body_was_requested() -> TestResult {
        let (_directory, service) = fixture()?;
        let with_body: GetSymbolParams =
            serde_json::from_value(json!({ "name": "signal", "include_body": true }))?;
        let without_body: GetSymbolParams =
            serde_json::from_value(json!({ "name": "signal", "include_body": false }))?;
        let with_body_value = serde_json::to_value(service.get_symbol(&with_body)?)?;
        let without_body_value = serde_json::to_value(service.get_symbol(&without_body)?)?;
        assert!(
            without_body_value["hits"][0].get("source").is_none(),
            "include_body: false must carry no source excerpt"
        );
        assert_eq!(
            with_body_value["hits"][0]["span"], without_body_value["hits"][0]["span"],
            "the span must not depend on include_body"
        );
        assert_eq!(
            without_body_value["hits"][0]["span"]["unit"],
            json!("rift://source/project/src/lib.rs")
        );
        assert!(
            without_body_value["hits"][0]["span"]["range"]["start"]
                .as_u64()
                .is_some_and(|start| start
                    < without_body_value["hits"][0]["span"]["range"]["end"]
                        .as_u64()
                        .unwrap_or_default()),
            "the span must name a real byte range: {without_body_value:#}"
        );
        Ok(())
    }

    /// The wire `Digest` truncates to eight characters, but the internal identity computation
    /// it truncates from keeps its full sixty-four-character SHA-256: the `stale_index`
    /// comparison and any future identity comparison work off the strong hash, not the short
    /// wire witness.
    #[test]
    fn workspace_digest_keeps_its_full_hash_before_wire_truncation() -> TestResult {
        let (_directory, service) = fixture()?;
        let full = super::workspace_digest(service.index());
        assert_eq!(full.len(), 64);
        let wire = service.tree_revision();
        assert_eq!(wire.len(), 8);
        assert!(full.starts_with(wire));
        Ok(())
    }

    /// Equal index and capture digests warn nothing: the answer's index covers the tree the
    /// read captured.
    #[test]
    fn captured_revisions_matching_digests_carry_no_warnings() {
        let digest = "aa".repeat(32);
        let revisions = super::CapturedRevisions {
            tree_revision: digest.clone(),
            index_tree_revision: digest,
        };
        assert_eq!(revisions.warnings(), Vec::new());
    }

    /// A lagging index emits one `stale_index` warning carrying both wire digests, so the
    /// caller sees which two trees disagree.
    #[test]
    fn captured_revisions_lagging_index_emits_stale_index_with_both_digests() -> TestResult {
        let revisions = super::CapturedRevisions {
            tree_revision: "bb".repeat(32),
            index_tree_revision: "aa".repeat(32),
        };
        let warnings = serde_json::to_value(revisions.warnings())?;
        assert_eq!(warnings.as_array().map(Vec::len), Some(1));
        let warning = &warnings[0];
        assert_eq!(warning["code"], json!("stale_index"));
        assert_eq!(warning["index_tree_revision"], json!("aaaaaaaa"));
        assert_eq!(warning["captured_tree_revision"], json!("bbbbbbbb"));
        let detail = warning["detail"].as_str().ok_or("detail must be prose")?;
        assert!(
            detail.contains("aaaaaaaa") && detail.contains("bbbbbbbb"),
            "the detail must state both wire digests: {detail}"
        );
        Ok(())
    }

    #[test]
    fn symbol_read_returns_normalized_identity_and_contributions() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let symbol = &value["hits"][0]["symbol"];

        assert_eq!(symbol["name"], "Beacon");
        assert_eq!(symbol["visibility"], "pub");
        assert_eq!(symbol["resolution"], "established");
        let index_revision = symbol["index_revision"]
            .as_u64()
            .expect("index revision is an integer");
        assert_ne!(index_revision, 0);
        assert_eq!(symbol["contributions"][0]["provider"], "syntax");
        assert_eq!(symbol["contributions"][0]["publication"], index_revision);
        assert!(
            symbol["contributions"][0]["symbol"]
                .as_str()
                .is_some_and(|identity| !identity.is_empty())
        );
        assert!(
            symbol.get("disagreements").is_none(),
            "an established symbol with no disagreements must omit the member"
        );
        assert!(
            symbol["facets"]
                .as_array()
                .is_some_and(|facets| facets.contains(&json!("public")))
        );
        assert!(
            symbol["id"]
                .as_str()
                .is_some_and(|id| id.contains("/Beacon"))
        );
        assert_eq!(value["hits"][0]["source"]["text"], "pub struct Beacon;");
        assert_eq!(
            symbol["origin"]["unit"],
            json!("rift://source/project/src/lib.rs")
        );
        assert_eq!(
            value["pagination"],
            json!({ "page_index": 0, "total_pages": 1 })
        );
        Ok(())
    }

    /// `helper` is exact, `helper_alpha` is a name prefix, and `cafe_helper` is a qualified-
    /// name substring - the order `GetSymbolParams.name`'s own doc states: "An exact symbol
    /// name ranks first, then prefix matches, then qualified-name substrings."
    #[test]
    fn get_symbol_ranks_exact_then_prefix_then_substring() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn helper() {}\npub fn helper_alpha() {}\npub fn cafe_helper() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "helper", "limit": 10}))?;
        let result = service.get_symbol(&params)?;
        let names: Vec<&str> = result
            .hits
            .iter()
            .map(|hit| hit.symbol.name.as_str())
            .collect();
        assert_eq!(names, ["helper", "helper_alpha", "cafe_helper"]);
        Ok(())
    }

    /// Pins the serialized symbol and node shape: the document's generic
    /// kind, facet, visibility, and container fields must serve the exact
    /// bytes the per-kind helpers served before them.
    #[test]
    fn symbol_and_node_wire_shape_is_unchanged() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "signal", "include_body": true}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;

        let symbol = &value["hits"][0]["symbol"];
        assert_eq!(symbol["language"], json!({ "name": "rust" }));
        assert_eq!(symbol["kind"], json!("rust.function"));
        assert_eq!(symbol["facets"], json!(["value", "callable", "public"]));
        assert_eq!(symbol["visibility"], json!("pub"));
        assert_eq!(
            symbol["container"],
            json!("rift://symbol/rust/src/lib.rs/Beacon")
        );

        let node = &value["hits"][0]["node"];
        assert_eq!(node["language"], json!({ "name": "rust" }));
        assert_eq!(node["kind"], json!("rust.function_item"));
        assert_eq!(node["facets"], json!(["declaration", "definition"]));

        let top_level: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let top_value = serde_json::to_value(service.get_symbol(&top_level)?)?;
        let beacon = &top_value["hits"][0]["symbol"];
        assert_eq!(beacon["kind"], json!("rust.struct"));
        assert_eq!(beacon["facets"], json!(["type", "public"]));
        assert!(
            beacon.get("container").is_none(),
            "a top-level declaration serves no container"
        );
        Ok(())
    }

    /// `projection` left `NodesParams`'s served fields; a request naming it is refused as an
    /// unknown field, not accepted and silently ignored.
    #[test]
    fn nodes_rejects_projection_as_an_unknown_field() {
        let result: Result<NodesParams, _> = serde_json::from_value(json!({
            "path": "src/lib.rs",
            "position": 0,
            "projection": "rift://projection/my-feature-one"
        }));
        assert!(
            result.is_err(),
            "a withdrawn projection field must fail deserialization"
        );
    }

    #[test]
    fn symbol_history_on_an_unversioned_workspace_is_a_history_fault() -> TestResult {
        let (_directory, service) = fixture()?;
        let mut params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        params.include_history = true;
        let error = service
            .get_symbol(&params)
            .expect_err("a workspace without a repository cannot serve history");
        assert!(matches!(error.fault(), ReadFault::History(_)));
        assert_eq!(error.descriptor().code(), "capability_unavailable");
        Ok(())
    }

    #[test]
    fn symbol_history_with_the_provider_disabled_is_unsupported() -> TestResult {
        let directory = committed_fixture()?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration {
                enabled: false,
                max_revisions: 500,
            },
        )?;
        let mut params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;
        params.include_history = true;
        let error = service
            .get_symbol(&params)
            .expect_err("a disabled history provider must refuse");
        let ReadFault::Unsupported { capability } = error.fault() else {
            panic!("expected Unsupported, got {:?}", error.fault());
        };
        assert_eq!(capability, "symbol history (providers.history disabled)");
        Ok(())
    }

    /// One symbol changed four ways across four commits: introduced, body
    /// grown, signature widened, decorated. The timeline lists them newest
    /// first with the classifier's kinds.
    fn timeline_fixture() -> TestResult<TempDir> {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        rift_history::fixture::commit_all(directory.path(), "introduce beacon");
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn beacon() { let _shift = 1; }\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "grow beacon body");
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn beacon() -> u8 { 7 }\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "widen beacon signature");
        fs::write(
            directory.path().join("src/lib.rs"),
            "#[inline]\npub fn beacon() -> u8 { 7 }\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "decorate beacon");
        Ok(directory)
    }

    #[test]
    fn symbol_history_lists_the_committed_timeline_newest_first() -> TestResult {
        let directory = timeline_fixture()?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "beacon", "include_history": true}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let history = &value["hits"][0]["history"];
        assert_eq!(
            history["symbol"],
            json!("rift://symbol/rust/src/lib.rs/beacon")
        );
        let versions = history["versions"]
            .as_array()
            .ok_or("history must carry versions")?;
        let kinds: Vec<&str> = versions
            .iter()
            .filter_map(|version| version["kind"].as_str())
            .collect();
        assert_eq!(
            kinds,
            [
                "decorators_changed",
                "signature_changed",
                "body_changed",
                "introduced"
            ]
        );
        let summaries: Vec<&str> = versions
            .iter()
            .filter_map(|version| version["summary"].as_str())
            .collect();
        assert_eq!(
            summaries,
            [
                "decorate beacon",
                "widen beacon signature",
                "grow beacon body",
                "introduce beacon"
            ]
        );
        for version in versions {
            assert_eq!(version["path"], json!("src/lib.rs"));
            assert_eq!(version["timestamp"], json!("2026-01-01T00:00:00+00:00"));
            assert_eq!(
                version["revision"].as_str().map(str::len),
                Some(40),
                "a served revision is the full hex commit id"
            );
        }
        Ok(())
    }

    #[test]
    fn symbol_history_revision_read_starts_at_the_addressed_commit() -> TestResult {
        let directory = timeline_fixture()?;
        rift_history::fixture::git(directory.path(), &["tag", "grown", "main~2"]);
        let service = revision_service(directory.path(), "grown")?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "beacon", "include_history": true}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let versions = value["hits"][0]["history"]["versions"]
            .as_array()
            .ok_or("history must carry versions")?;
        let kinds: Vec<&str> = versions
            .iter()
            .filter_map(|version| version["kind"].as_str())
            .collect();
        assert_eq!(
            kinds,
            ["body_changed", "introduced"],
            "the walk starts at the addressed commit, not the branch head"
        );
        Ok(())
    }

    #[test]
    fn symbol_history_stays_off_the_wire_without_the_request_flag() -> TestResult {
        let directory = timeline_fixture()?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        assert!(
            value["hits"][0].get("history").is_none(),
            "an unrequested timeline never rides a hit"
        );
        Ok(())
    }

    #[test]
    fn nodes_missing_source_is_not_found() -> TestResult {
        let (_directory, service) = fixture()?;
        let missing = service.nodes(NodesParams {
            path: ProjectPath("src/missing.rs".to_owned()),
            position: 0,
            rev: None,
        });
        assert!(matches!(
            missing.expect_err("missing source must fail").fault(),
            ReadFault::NotFound { .. }
        ));
        Ok(())
    }

    /// One read service over `directory` under `visibility`, the shape every
    /// `nodes` classification test builds.
    fn nodes_service(
        directory: &std::path::Path,
        visibility: &SourceVisibility,
    ) -> Result<ReadService, super::ReadError> {
        let limits = WorkspaceIndexLimits::default();
        let inclusion = rift_core::TextFileInclusion::default();
        ReadService::build(
            directory,
            limits,
            visibility,
            &inclusion,
            HistoryConfiguration::default(),
        )
    }

    fn nodes_at_root(service: &ReadService, path: &str) -> Result<NodesResult, super::ReadError> {
        service.nodes(NodesParams {
            path: ProjectPath(path.to_owned()),
            position: 0,
            rev: None,
        })
    }

    #[test]
    fn nodes_on_an_unparsed_path_names_its_extension_without_configuration_advice() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("Cargo.lock"), "# generated\n")?;
        let service = nodes_service(directory.path(), &SourceVisibility::default())?;
        let error = nodes_at_root(&service, "Cargo.lock")
            .expect_err("an unparsed extension must be rejected");
        let ReadFault::UnclaimedExtension { extension } = error.fault() else {
            panic!("expected UnclaimedExtension, got {:?}", error.fault());
        };
        assert_eq!(extension, "lock files", "the refusal names the extension");
        assert_eq!(error.descriptor().code(), "capability_unavailable");
        assert!(
            !error.to_string().contains("configure a provider"),
            "no configuration can ever add a shipped grammar, so the message must not \
             suggest one: {error}"
        );
        Ok(())
    }

    #[test]
    fn nodes_on_a_source_excluded_unparsed_path_stays_not_found() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("Cargo.lock"), "# generated\n")?;
        let visibility = SourceVisibility::new(Vec::new(), vec!["Cargo.lock".to_owned()], false);
        let service = nodes_service(directory.path(), &visibility)?;
        let error =
            nodes_at_root(&service, "Cargo.lock").expect_err("an excluded path must be rejected");
        assert!(
            matches!(error.fault(), ReadFault::NotFound { .. }),
            "the workspace asked for this path to be invisible, so nodes cannot name a \
             capability for it: {:?}",
            error.fault()
        );
        Ok(())
    }

    #[test]
    fn nodes_on_an_absent_unparsed_path_stays_not_found() -> TestResult {
        let directory = tempfile::tempdir()?;
        let service = nodes_service(directory.path(), &SourceVisibility::default())?;
        let error =
            nodes_at_root(&service, "absent.lock").expect_err("an absent path must be rejected");
        assert!(
            matches!(error.fault(), ReadFault::NotFound { .. }),
            "no file stands at that path, so there is no capability to name: {:?}",
            error.fault()
        );
        Ok(())
    }

    #[test]
    fn nodes_on_a_visible_unparsed_path_names_the_extension() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("justfile"), "default:\n    echo hi\n")?;
        let service = nodes_service(directory.path(), &SourceVisibility::default())?;
        let error = nodes_at_root(&service, "justfile")
            .expect_err("an unparsed extension must be rejected");
        let ReadFault::UnclaimedExtension { extension } = error.fault() else {
            panic!("expected UnclaimedExtension, got {:?}", error.fault());
        };
        assert_eq!(
            extension, "files with no extension",
            "justfile carries no extension at all"
        );
        Ok(())
    }

    /// A workspace holding one UTF-8-invalid source file beside a valid one: addressing
    /// the invalid file directly answers `content_unavailable`, a genuinely absent sibling
    /// path still answers `not_found`, and reading the valid file still serves normally
    /// while carrying a warning naming the invalid one.
    #[test]
    fn nodes_distinguishes_invalid_utf8_from_absent_and_still_serves_and_warns() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn kept() {}\n")?;
        fs::write(directory.path().join("src/invalid.rs"), [0xff])?;
        let service = nodes_service(directory.path(), &SourceVisibility::default())?;

        let invalid = nodes_at_root(&service, "src/invalid.rs")
            .expect_err("the addressed file exists but cannot be read");
        let invalid_fault = invalid.fault();
        assert!(
            matches!(invalid_fault, ReadFault::SourceUnavailable { .. }),
            "an invalid-UTF-8 path answers content_unavailable, not not_found: {invalid_fault:?}"
        );
        assert_eq!(
            invalid.descriptor().code(),
            "content_unavailable",
            "the wire code names the omitted content, not a missing path"
        );

        let absent = nodes_at_root(&service, "src/missing.rs")
            .expect_err("a path nothing claims must still fail");
        let absent_fault = absent.fault();
        assert!(
            matches!(absent_fault, ReadFault::NotFound { .. }),
            "a genuinely absent sibling path is unaffected: {absent_fault:?}"
        );
        assert_eq!(
            absent.descriptor().code(),
            "resource_not_found",
            "a genuinely absent path keeps its own wire code"
        );

        let kept = nodes_at_root(&service, "src/lib.rs")?;
        let invalid_path = rift_core::ProjectPath::new("src/invalid.rs")?;
        assert!(
            kept.warnings.contains(&ReadWarning::SourceUnavailable {
                unit: file_id(&invalid_path),
                detail: "src/invalid.rs's bytes are not valid UTF-8, so the file is absent \
                         from the index"
                    .to_owned(),
            }),
            "the valid file still serves and its result names the skipped one: {:?}",
            kept.warnings
        );
        Ok(())
    }

    #[test]
    fn get_symbol_over_a_workspace_with_an_invalid_utf8_file_still_warns_and_serves_others()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub struct Beacon;\n")?;
        fs::write(directory.path().join("src/invalid.rs"), [0xff])?;
        let service = nodes_service(directory.path(), &SourceVisibility::default())?;

        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let result = service.get_symbol(&params)?;
        assert_eq!(
            result.hits.len(),
            1,
            "the valid file's declaration is still found"
        );
        let invalid_path = rift_core::ProjectPath::new("src/invalid.rs")?;
        assert!(
            result.warnings.contains(&ReadWarning::SourceUnavailable {
                unit: file_id(&invalid_path),
                detail: "src/invalid.rs's bytes are not valid UTF-8, so the file is absent \
                         from the index"
                    .to_owned(),
            }),
            "get_symbol's answer names the skipped file too: {:?}",
            result.warnings
        );
        Ok(())
    }

    #[test]
    fn invalid_root_preserves_index_error_source() {
        let error = ReadService::build(
            std::path::Path::new("not-a-real-rift-workspace"),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
        .expect_err("missing root must fail");

        assert!(matches!(error.fault(), ReadFault::Index(_)));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn nodes_rejects_path_outside_project_root() -> TestResult {
        let (_directory, service) = fixture()?;
        let result = service.nodes(NodesParams {
            path: ProjectPath("/etc/passwd".to_owned()),
            position: 0,
            rev: None,
        });
        assert!(matches!(
            result.expect_err("absolute path must fail").fault(),
            ReadFault::Invalid { .. }
        ));
        Ok(())
    }

    /// `scope` left `GetSymbolParams`'s served fields once `project` became its only value; a
    /// request naming it is refused as an unknown field, not accepted and silently ignored.
    #[test]
    fn get_symbol_rejects_scope_as_an_unknown_field() {
        let result: Result<GetSymbolParams, _> =
            serde_json::from_value(json!({"name": "Beacon", "scope": "dependencies"}));
        assert!(
            result.is_err(),
            "a withdrawn scope field must fail deserialization"
        );
    }

    #[test]
    fn nodes_facets_identify_expression_statement_and_comment() -> TestResult {
        let (_directory, service) = rich_fixture()?;

        let expression_position = RICH_SOURCE
            .find("1 + 2")
            .ok_or("fixture must contain expression")? as u64
            + 2;
        let expression = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: expression_position,
            rev: None,
        })?;
        assert!(any_node_has_facet(&expression, "expression")?);

        let statement_position = RICH_SOURCE
            .find("total;")
            .ok_or("fixture must contain statement")? as u64;
        let statement = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: statement_position,
            rev: None,
        })?;
        assert!(any_node_has_facet(&statement, "statement")?);

        let comment_position = RICH_SOURCE
            .find("lookout")
            .ok_or("fixture must contain comment")? as u64;
        let comment = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: comment_position,
            rev: None,
        })?;
        assert!(any_node_has_facet(&comment, "comment")?);

        Ok(())
    }

    #[test]
    fn storage_fault_renders_path_operation_and_io_in_order() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "sealed");
        let error = ReadFault::storage("src/lib.rs", "stage", &io);
        assert_eq!(error.descriptor().code(), "storage_failure");
        let context = error.context();
        let keys: Vec<&str> = context.iter().map(rift_core::ErrorContext::key).collect();
        assert_eq!(keys, ["path", "operation", "io"]);
        assert_eq!(context[0].value(), "src/lib.rs");
        assert_eq!(context[2].value(), "sealed");
    }

    #[test]
    fn task_fault_is_internal_and_names_the_blocking_operation() {
        let error = ReadFault::task("initial index build", "worker panicked");
        assert_eq!(error.descriptor().code(), "internal_error");
        let context = error.context();
        assert_eq!(context[0].key(), "operation");
        assert_eq!(context[0].value(), "initial index build");
        assert_eq!(context[1].key(), "detail");
        assert_eq!(context[1].value(), "worker panicked");
    }

    #[test]
    fn wire_semantic_enums_map_every_internal_value() {
        let resolutions = [
            (
                rift_core::SymbolResolution::Established,
                rift_protocol::read::SymbolResolution::Established,
            ),
            (
                rift_core::SymbolResolution::Unresolved,
                rift_protocol::read::SymbolResolution::Unresolved,
            ),
            (
                rift_core::SymbolResolution::Conflicting,
                rift_protocol::read::SymbolResolution::Conflicting,
            ),
        ];
        for (internal, wire) in resolutions {
            assert_eq!(super::wire_symbol_resolution(internal), wire);
        }

        let fields = [
            (
                rift_provider::PresentationField::Language,
                rift_protocol::read::SymbolPresentationField::Language,
            ),
            (
                rift_provider::PresentationField::Name,
                rift_protocol::read::SymbolPresentationField::Name,
            ),
            (
                rift_provider::PresentationField::QualifiedName,
                rift_protocol::read::SymbolPresentationField::QualifiedName,
            ),
            (
                rift_provider::PresentationField::Kind,
                rift_protocol::read::SymbolPresentationField::Kind,
            ),
            (
                rift_provider::PresentationField::Container,
                rift_protocol::read::SymbolPresentationField::Container,
            ),
            (
                rift_provider::PresentationField::Visibility,
                rift_protocol::read::SymbolPresentationField::Visibility,
            ),
            (
                rift_provider::PresentationField::DocumentLocal,
                rift_protocol::read::SymbolPresentationField::DocumentLocal,
            ),
            (
                rift_provider::PresentationField::Origin,
                rift_protocol::read::SymbolPresentationField::Origin,
            ),
        ];
        for (internal, wire) in fields {
            assert_eq!(super::wire_presentation_field(internal), wire);
        }
    }

    #[test]
    fn nodes_backlink_documented_item_to_its_symbol() -> TestResult {
        let (_directory, service) = documented_fixture()?;
        let position = DOCUMENTED_SOURCE
            .find("Beacon")
            .ok_or("fixture must contain the struct name")? as u64;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position,
            rev: None,
        })?;
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        let item = nodes
            .iter()
            .find(|node| node["kind"] == "rust.struct_item")
            .ok_or("fixture must witness the struct_item node")?;
        assert!(
            item["symbol"]
                .as_str()
                .is_some_and(|id| id.contains("/Beacon")),
            "documented struct item must backlink to its symbol, got {:?}",
            item["symbol"]
        );
        Ok(())
    }

    #[test]
    fn nodes_backlink_undocumented_item_to_its_symbol() -> TestResult {
        let (_directory, service) = fixture()?;
        let position = "pub struct ".len() as u64;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position,
            rev: None,
        })?;
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        let item = nodes
            .iter()
            .find(|node| node["kind"] == "rust.struct_item")
            .ok_or("fixture must witness the struct_item node")?;
        assert!(
            item["symbol"]
                .as_str()
                .is_some_and(|id| id.contains("/Beacon")),
            "undocumented struct item must still backlink to its symbol"
        );
        Ok(())
    }

    #[test]
    fn nodes_report_no_symbol_backlink_for_non_symbol_node() -> TestResult {
        let (_directory, service) = fixture()?;
        let position = "pub struct Beacon;\n".len() as u64;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position,
            rev: None,
        })?;
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        let impl_node = nodes
            .iter()
            .find(|node| node["kind"] == "rust.impl_item")
            .ok_or("fixture must witness the impl_item node")?;
        assert!(
            impl_node.get("symbol").is_none(),
            "impl_item is not itself a declared symbol, so the member stays off the wire"
        );
        Ok(())
    }

    /// One workspace holding every shipped source language.
    fn multi_language_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("src/routes.ts"),
            "export interface Route {\n  path: string;\n}\n",
        )?;
        fs::write(
            directory.path().join("src/App.tsx"),
            "export function App() {\n  return <main>beacon</main>;\n}\n",
        )?;
        fs::write(
            directory.path().join("src/banner.js"),
            "export function banner() { return 1; }\n",
        )?;
        let guide_md = "# Beacon Guide\n\nHow the beacon works.\n";
        fs::write(directory.path().join("src/guide.md"), guide_md)?;
        let settings_json = "{\"beacon settings\": {\"port\": 8080}}\n";
        fs::write(directory.path().join("settings.json"), settings_json)?;
        let pipeline_yaml = "beacon pipeline:\n  retries: 3\n";
        fs::write(directory.path().join("pipeline.yaml"), pipeline_yaml)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    #[test]
    fn get_symbol_finds_a_typescript_interface_beside_other_languages() -> TestResult {
        let (_directory, service) = multi_language_fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Route"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let symbol = &value["hits"][0]["symbol"];
        assert_eq!(symbol["language"], json!({ "name": "typescript" }));
        assert_eq!(symbol["kind"], json!("typescript.interface"));
        assert_eq!(symbol["facets"], json!(["type", "public"]));
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/typescript/src/routes.ts/Route")
        );
        assert_eq!(
            value["hits"][0]["source"]["text"],
            "interface Route {\n  path: string;\n}"
        );
        Ok(())
    }

    /// One name declared in two languages; the `language` filter narrows the
    /// hits and the pagination counts the filtered set.
    #[test]
    fn get_symbol_language_filter_narrows_the_hits() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("src/beacon.ts"),
            "export function beacon() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let unfiltered: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;
        assert_eq!(service.get_symbol(&unfiltered)?.hits.len(), 2);
        let filtered: GetSymbolParams =
            serde_json::from_value(json!({"name": "beacon", "language": {"name": "rust"}}))?;
        let result = service.get_symbol(&filtered)?;
        let value = serde_json::to_value(&result)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(
            value["hits"][0]["symbol"]["language"],
            json!({"name": "rust"})
        );
        assert_eq!(
            value["pagination"],
            json!({"page_index": 0, "total_pages": 1})
        );
        let dialect_filtered: GetSymbolParams = serde_json::from_value(json!({
            "name": "beacon",
            "language": {"name": "typescript", "dialect": "tsx"}
        }))?;
        assert!(
            service.get_symbol(&dialect_filtered)?.hits.is_empty(),
            "a dialect-stated filter must not select the dialect-free typescript document"
        );
        Ok(())
    }

    /// A filter without a dialect selects every dialect of its name.
    #[test]
    fn get_symbol_language_filter_without_a_dialect_selects_every_dialect() -> TestResult {
        let (_directory, service) = multi_language_fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({
            "name": "App",
            "language": {"name": "typescript"}
        }))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        assert_eq!(
            value["hits"][0]["symbol"]["language"],
            json!({"name": "typescript", "dialect": "tsx"})
        );
        Ok(())
    }

    #[test]
    fn nodes_serve_a_tsx_file_under_the_typescript_wire_kinds() -> TestResult {
        let (directory, service) = multi_language_fixture()?;
        let source = fs::read_to_string(directory.path().join("src/App.tsx"))?;
        let position = source.find("<main>").ok_or("fixture must contain JSX")? as u64;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/App.tsx".to_owned()),
            position,
            rev: None,
        })?;
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        assert!(!nodes.is_empty());
        assert!(
            nodes.iter().all(|node| {
                node["kind"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("typescript."))
            }),
            "every wire kind composes from the language name: {nodes:#?}"
        );
        assert!(
            nodes
                .iter()
                .any(|node| node["kind"] == "typescript.jsx_element"),
            "position sits inside the JSX element: {nodes:#?}"
        );
        let jsx = nodes
            .iter()
            .find(|node| node["kind"] == "typescript.jsx_element")
            .ok_or("fixture must witness the jsx_element node")?;
        assert_eq!(
            jsx["language"],
            json!({ "name": "typescript", "dialect": "tsx" })
        );
        assert_eq!(jsx["facets"], json!(["expression"]));
        assert!(
            jsx["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("rift://node/typescript:tsx/")),
            "a tsx node address files under the dialect segment: {:?}",
            jsx["id"]
        );
        Ok(())
    }

    /// One TypeScript declaration introduced and then body-edited across two
    /// commits; the timeline classifies both through the typescript provider.
    #[test]
    fn typescript_symbol_history_lists_the_committed_timeline() -> TestResult {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/routes.ts"),
            "export function lookup(route: string): string {\n  return route;\n}\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "introduce lookup");
        fs::write(
            directory.path().join("src/routes.ts"),
            "export function lookup(route: string): string {\n  return route + \"/v2\";\n}\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "grow lookup body");
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "lookup", "include_history": true}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let history = &value["hits"][0]["history"];
        assert_eq!(
            history["symbol"],
            json!("rift://symbol/typescript/src/routes.ts/lookup")
        );
        let versions = history["versions"]
            .as_array()
            .ok_or("history must carry versions")?;
        let kinds: Vec<&str> = versions
            .iter()
            .filter_map(|version| version["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["body_changed", "introduced"]);
        let summaries: Vec<&str> = versions
            .iter()
            .filter_map(|version| version["summary"].as_str())
            .collect();
        assert_eq!(summaries, ["grow lookup body", "introduce lookup"]);
        Ok(())
    }

    /// A markdown heading answers `get_symbol` like any declaration: the
    /// composed wire kind, empty facets, an id escaping the heading text,
    /// and the whole section as the source excerpt.
    #[test]
    fn get_symbol_finds_a_markdown_heading_beside_other_languages() -> TestResult {
        let (_directory, service) = multi_language_fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon Guide"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let symbol = &value["hits"][0]["symbol"];
        assert_eq!(symbol["language"], json!({ "name": "markdown" }));
        assert_eq!(symbol["kind"], json!("markdown.heading"));
        assert!(
            symbol.get("facets").is_none(),
            "no facets must omit the member"
        );
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/markdown/src/guide.md/Beacon%20Guide")
        );
        assert_eq!(
            value["hits"][0]["source"]["text"],
            "# Beacon Guide\n\nHow the beacon works.\n"
        );
        Ok(())
    }

    #[test]
    fn nodes_serve_a_markdown_file_under_the_markdown_wire_kinds() -> TestResult {
        let (directory, service) = multi_language_fixture()?;
        let source = fs::read_to_string(directory.path().join("src/guide.md"))?;
        let position = source
            .find("beacon works")
            .ok_or("fixture must contain the prose line")? as u64;
        let params = NodesParams {
            path: ProjectPath("src/guide.md".to_owned()),
            position,
            rev: None,
        };
        let value = serde_json::to_value(service.nodes(params)?)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        assert!(!nodes.is_empty());
        assert!(
            nodes.iter().all(|node| {
                node["kind"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("markdown."))
            }),
            "every wire kind composes from the language name: {nodes:#?}"
        );
        let section = nodes
            .iter()
            .find(|node| node["kind"] == "markdown.section")
            .ok_or("position sits inside the heading's section")?;
        assert_eq!(section["language"], json!({ "name": "markdown" }));
        assert_eq!(section["facets"], json!(["declaration"]));
        let section_id = section["id"].as_str().unwrap_or_default();
        assert!(
            section_id.starts_with("rift://node/markdown/"),
            "a markdown node address files under the markdown segment: {section_id}"
        );
        assert!(
            nodes
                .iter()
                .any(|node| node["kind"] == "markdown.paragraph"),
            "position sits inside the prose paragraph: {nodes:#?}"
        );
        Ok(())
    }

    /// One heading introduced and then content-edited across two commits;
    /// the timeline classifies both through the markdown provider.
    #[test]
    fn markdown_symbol_history_lists_the_committed_timeline() -> TestResult {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        let introduced = "# Install\n\nRun the beacon.\n";
        fs::write(directory.path().join("docs.md"), introduced)?;
        rift_history::fixture::commit_all(directory.path(), "introduce install guide");
        let grown = "# Install\n\nRun the beacon twice.\n";
        fs::write(directory.path().join("docs.md"), grown)?;
        rift_history::fixture::commit_all(directory.path(), "grow install guide");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service =
            ReadService::build(directory.path(), limits, &visibility, &inclusion, history)?;
        let request = json!({"name": "Install", "include_history": true});
        let params: GetSymbolParams = serde_json::from_value(request)?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let history = &value["hits"][0]["history"];
        assert_eq!(
            history["symbol"],
            json!("rift://symbol/markdown/docs.md/Install")
        );
        let versions = history["versions"]
            .as_array()
            .ok_or("history must carry versions")?;
        let kinds: Vec<&str> = versions
            .iter()
            .filter_map(|version| version["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["body_changed", "introduced"]);
        let summaries: Vec<&str> = versions
            .iter()
            .filter_map(|version| version["summary"].as_str())
            .collect();
        assert_eq!(summaries, ["grow install guide", "introduce install guide"]);
        Ok(())
    }

    /// A JSON member answers `get_symbol` like any declaration: the
    /// composed wire kind, empty facets, an id escaping the key, and the
    /// whole pair as the source excerpt.
    #[test]
    fn get_symbol_finds_a_json_member_beside_other_languages() -> TestResult {
        let (_directory, service) = multi_language_fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon settings"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let symbol = &value["hits"][0]["symbol"];
        assert_eq!(symbol["language"], json!({ "name": "json" }));
        assert_eq!(symbol["kind"], json!("json.member"));
        assert!(
            symbol.get("facets").is_none(),
            "no facets must omit the member"
        );
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/json/settings.json/beacon%20settings")
        );
        assert_eq!(
            value["hits"][0]["source"]["text"],
            "\"beacon settings\": {\"port\": 8080}"
        );

        let nested: GetSymbolParams = serde_json::from_value(json!({
            "name": "port",
            "language": {"name": "json"}
        }))?;
        let value = serde_json::to_value(service.get_symbol(&nested)?)?;
        assert_eq!(
            value["hits"][0]["symbol"]["id"],
            json!("rift://symbol/json/settings.json/beacon%20settings%20%3E%20port"),
            "a nested member's id escapes its whole key path"
        );
        Ok(())
    }

    /// A YAML mapping entry answers `get_symbol` like any declaration: the
    /// composed wire kind, empty facets, an id escaping the key, and the
    /// whole pair as the source excerpt.
    #[test]
    fn get_symbol_finds_a_yaml_entry_beside_other_languages() -> TestResult {
        let (_directory, service) = multi_language_fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon pipeline"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let symbol = &value["hits"][0]["symbol"];
        assert_eq!(symbol["language"], json!({ "name": "yaml" }));
        assert_eq!(symbol["kind"], json!("yaml.mapping_entry"));
        assert!(
            symbol.get("facets").is_none(),
            "no facets must omit the member"
        );
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/yaml/pipeline.yaml/beacon%20pipeline")
        );
        assert_eq!(
            value["hits"][0]["source"]["text"], "beacon pipeline:\n  retries: 3\n",
            "the excerpt serves whole lines, so the pair's last line ends it"
        );
        Ok(())
    }

    #[test]
    fn nodes_serve_a_json_file_under_the_json_wire_kinds() -> TestResult {
        let (directory, service) = multi_language_fixture()?;
        let source = fs::read_to_string(directory.path().join("settings.json"))?;
        let position = source.find("8080").ok_or("fixture must contain the port")? as u64;
        let params = NodesParams {
            path: ProjectPath("settings.json".to_owned()),
            position,
            rev: None,
        };
        let value = serde_json::to_value(service.nodes(params)?)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        assert!(!nodes.is_empty());
        assert!(
            nodes.iter().all(|node| {
                node["kind"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("json."))
            }),
            "every wire kind composes from the language name: {nodes:#?}"
        );
        let pair = nodes
            .iter()
            .find(|node| node["kind"] == "json.pair")
            .ok_or("position sits inside the port member's pair")?;
        assert_eq!(pair["language"], json!({ "name": "json" }));
        assert_eq!(pair["facets"], json!(["declaration"]));
        let pair_id = pair["id"].as_str().unwrap_or_default();
        assert!(
            pair_id.starts_with("rift://node/json/"),
            "a JSON node address files under the json segment: {pair_id}"
        );
        assert!(
            nodes.iter().any(|node| node["kind"] == "json.number"),
            "position sits inside the number value: {nodes:#?}"
        );
        Ok(())
    }

    #[test]
    fn nodes_serve_a_yaml_file_under_the_yaml_wire_kinds() -> TestResult {
        let (directory, service) = multi_language_fixture()?;
        let source = fs::read_to_string(directory.path().join("pipeline.yaml"))?;
        let position = source
            .find("retries")
            .ok_or("fixture must contain the entry")? as u64;
        let params = NodesParams {
            path: ProjectPath("pipeline.yaml".to_owned()),
            position,
            rev: None,
        };
        let value = serde_json::to_value(service.nodes(params)?)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        assert!(!nodes.is_empty());
        assert!(
            nodes.iter().all(|node| {
                node["kind"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("yaml."))
            }),
            "every wire kind composes from the language name: {nodes:#?}"
        );
        let pair = nodes
            .iter()
            .find(|node| node["kind"] == "yaml.block_mapping_pair")
            .ok_or("position sits inside the retries entry's pair")?;
        assert_eq!(pair["language"], json!({ "name": "yaml" }));
        assert_eq!(pair["facets"], json!(["declaration"]));
        let pair_id = pair["id"].as_str().unwrap_or_default();
        assert!(
            pair_id.starts_with("rift://node/yaml/"),
            "a YAML node address files under the yaml segment: {pair_id}"
        );
        Ok(())
    }

    /// One JSON member introduced and then value-edited across two commits;
    /// the timeline classifies both through the JSON provider.
    #[test]
    fn json_symbol_history_lists_the_committed_timeline() -> TestResult {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::write(
            directory.path().join("settings.json"),
            "{\"server\": {\"port\": 1}}\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "introduce settings");
        fs::write(
            directory.path().join("settings.json"),
            "{\"server\": {\"port\": 2}}\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "grow settings");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service =
            ReadService::build(directory.path(), limits, &visibility, &inclusion, history)?;
        let request = json!({"name": "server", "include_history": true});
        let params: GetSymbolParams = serde_json::from_value(request)?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let history = &value["hits"][0]["history"];
        assert_eq!(
            history["symbol"],
            json!("rift://symbol/json/settings.json/server")
        );
        let kinds: Vec<&str> = history["versions"]
            .as_array()
            .ok_or("history must carry versions")?
            .iter()
            .filter_map(|version| version["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["body_changed", "introduced"]);
        Ok(())
    }

    /// One YAML entry introduced and then value-edited across two commits
    /// in a `.yml` file; the timeline classifies both through the YAML
    /// provider.
    #[test]
    fn yaml_symbol_history_lists_the_committed_timeline() -> TestResult {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::write(directory.path().join("deploy.yml"), "retries: 3\n")?;
        rift_history::fixture::commit_all(directory.path(), "introduce deploy");
        fs::write(directory.path().join("deploy.yml"), "retries: 5\n")?;
        rift_history::fixture::commit_all(directory.path(), "grow deploy");
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service =
            ReadService::build(directory.path(), limits, &visibility, &inclusion, history)?;
        let request = json!({"name": "retries", "include_history": true});
        let params: GetSymbolParams = serde_json::from_value(request)?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let history = &value["hits"][0]["history"];
        assert_eq!(
            history["symbol"],
            json!("rift://symbol/yaml/deploy.yml/retries")
        );
        let kinds: Vec<&str> = history["versions"]
            .as_array()
            .ok_or("history must carry versions")?
            .iter()
            .filter_map(|version| version["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["body_changed", "introduced"]);
        Ok(())
    }

    /// One committed source file, then uncommitted working-tree drift on top
    /// of it, so a revision read and a working-tree read answer differently.
    fn committed_fixture() -> TestResult<TempDir> {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        rift_history::fixture::commit_all(directory.path(), "introduce beacon");
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn beacon() -> u8 {\n    7\n}\n",
        )?;
        Ok(directory)
    }

    fn revision_service(
        root: &std::path::Path,
        rev: &str,
    ) -> Result<ReadService, super::ReadError> {
        ReadService::at_revision(
            root,
            &RevisionId(rev.to_owned()),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            HistoryConfiguration::default(),
        )
    }

    #[test]
    fn a_revision_snapshot_names_the_capture_it_refuses() -> TestResult {
        let directory = committed_fixture()?;
        let service = revision_service(directory.path(), "main")?;

        let entries = service
            .capture_visible_workspace_entries()
            .expect_err("a revision snapshot has no filesystem tree to walk");
        let digests = service
            .visible_workspace_digests()
            .expect_err("a revision snapshot has no filesystem tree to digest");

        let entries_fault = entries.fault();
        assert!(
            matches!(
                entries_fault,
                ReadFault::Task { operation, .. } if *operation == "capture visible workspace entries"
            ),
            "unexpected fault {entries_fault:?}"
        );
        let digests_fault = digests.fault();
        assert!(
            matches!(
                digests_fault,
                ReadFault::Task { operation, .. } if *operation == "capture visible workspace digests"
            ),
            "unexpected fault {digests_fault:?}"
        );
        Ok(())
    }

    #[test]
    fn revision_nodes_on_an_unparsed_path_names_the_extension_without_a_policy() -> TestResult {
        let directory = committed_fixture()?;
        let service = revision_service(directory.path(), "main")?;
        let error = service
            .nodes(NodesParams {
                path: ProjectPath("Cargo.lock".to_owned()),
                position: 0,
                rev: Some(RevisionId("main".to_owned())),
            })
            .expect_err("an unparsed extension must be rejected at a revision too");
        let ReadFault::UnclaimedExtension { extension } = error.fault() else {
            panic!("expected UnclaimedExtension, got {:?}", error.fault());
        };
        assert_eq!(
            extension, "lock files",
            "a revision snapshot carries no filesystem policy, so the extension alone \
             decides: nodes can never serve an unclaimed one, whatever tree it reads"
        );
        Ok(())
    }

    #[test]
    fn revision_read_serves_the_committed_declaration() -> TestResult {
        let directory = committed_fixture()?;
        let service = revision_service(directory.path(), "main")?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        assert_eq!(
            value["hits"][0]["source"]["text"], "pub fn beacon() {}",
            "the committed body answers, not the drifted working tree"
        );
        assert!(
            value.get("warnings").is_none(),
            "a live get_symbol result must omit warnings when there is nothing to warn about"
        );
        Ok(())
    }

    #[test]
    fn revision_tree_digest_differs_from_the_drifted_working_tree() -> TestResult {
        let directory = committed_fixture()?;
        let at_head = revision_service(directory.path(), "main")?;
        let working = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        assert_ne!(
            at_head.tree_revision(),
            working.tree_revision(),
            "drifted bytes must produce a different tree digest"
        );
        assert_eq!(
            working.revision(),
            None,
            "a working-tree read serves no resolved commit"
        );
        Ok(())
    }

    #[test]
    fn revision_nodes_list_committed_syntax() -> TestResult {
        let directory = committed_fixture()?;
        let service = revision_service(directory.path(), "main")?;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 8,
            rev: Some(RevisionId("main".to_owned())),
        })?;
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        assert!(
            nodes
                .iter()
                .any(|node| node["kind"] == "rust.function_item"),
            "position 8 sits inside the committed `pub fn beacon`"
        );
        Ok(())
    }

    #[test]
    fn revision_read_refuses_an_unversioned_workspace_with_the_actionable_message() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let error = revision_service(directory.path(), "main")
            .expect_err("a workspace without a repository must refuse");
        assert_eq!(error.descriptor().code(), "capability_unavailable");
        let canonical = fs::canonicalize(directory.path())?;
        assert_eq!(
            error.to_string(),
            format!(
                "no configured provider serves this request: workspace {}, \
                 requires a git repository - run `git init`, or omit `rev` to \
                 read the current tree; adjust the request to a served \
                 capability, or configure a provider that serves it",
                canonical.display()
            )
        );
        Ok(())
    }

    #[test]
    fn revision_read_refuses_an_unknown_revision_as_not_found() -> TestResult {
        let directory = committed_fixture()?;
        let error = revision_service(directory.path(), "feature/absent")
            .expect_err("an unknown revision must refuse");
        assert_eq!(error.descriptor().code(), "resource_not_found");
        Ok(())
    }

    #[test]
    fn revision_read_refuses_a_forbidden_spelling_as_invalid() -> TestResult {
        let directory = committed_fixture()?;
        let error = revision_service(directory.path(), "HEAD~1")
            .expect_err("a spelling outside the advertised charset must refuse");
        assert_eq!(
            error.to_string(),
            "the request does not match the documented form: field rev, \
             violation charset_forbidden; correct the reported field and \
             resend the request"
        );
        Ok(())
    }

    #[test]
    fn page_of_an_empty_set_reports_zero_total_pages() {
        let (window, pagination) = super::page(Vec::<u8>::new(), 0, 5);
        assert_eq!(window, Vec::<u8>::new());
        assert_eq!(pagination.page_index, 0);
        assert_eq!(pagination.total_pages, 0);
    }

    #[test]
    fn page_zero_serves_the_first_window_by_default() {
        let (window, pagination) = super::page(vec![1, 2, 3, 4, 5], 0, 2);
        assert_eq!(window, vec![1, 2]);
        assert_eq!(pagination.page_index, 0);
        assert_eq!(pagination.total_pages, 3);
    }

    #[test]
    fn page_count_is_exact_for_a_set_that_divides_evenly() {
        let (window, pagination) = super::page(vec![1, 2, 3, 4, 5, 6], 1, 3);
        assert_eq!(window, vec![4, 5, 6]);
        assert_eq!(pagination.total_pages, 2);
    }

    #[test]
    fn page_count_rounds_up_and_the_last_page_carries_the_remainder() {
        let (window, pagination) = super::page(vec![1, 2, 3, 4, 5, 6, 7], 2, 3);
        assert_eq!(window, vec![7]);
        assert_eq!(pagination.total_pages, 3);
    }

    #[test]
    fn page_past_the_end_is_empty_and_keeps_the_true_page_count() {
        let (window, pagination) = super::page(vec![1, 2, 3], 9, 2);
        assert_eq!(window, Vec::<i32>::new());
        assert_eq!(pagination.page_index, 9);
        assert_eq!(pagination.total_pages, 2);
    }

    /// The window offset is `page_index * limit` at checked boundaries: an index whose
    /// offset cannot be represented is past every collectable page, never a panic.
    #[test]
    fn page_index_beyond_arithmetic_range_is_an_empty_page() {
        let (window, pagination) = super::page(vec![1, 2, 3], u64::MAX, usize::MAX);
        assert_eq!(window, Vec::<i32>::new());
        assert_eq!(pagination.page_index, u64::MAX);
        assert_eq!(pagination.total_pages, 1);
    }

    #[test]
    fn get_symbol_pages_walk_the_full_match_set_without_overlap() -> TestResult {
        let (_directory, service) = fixture()?;
        let first: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon", "limit": 1}))?;
        let first_value = serde_json::to_value(service.get_symbol(&first)?)?;
        assert_eq!(
            first_value["pagination"],
            json!({ "page_index": 0, "total_pages": 2 })
        );
        let second: GetSymbolParams =
            serde_json::from_value(json!({"name": "Beacon", "limit": 1, "page_index": 1}))?;
        let second_value = serde_json::to_value(service.get_symbol(&second)?)?;
        assert_eq!(
            second_value["pagination"],
            json!({ "page_index": 1, "total_pages": 2 })
        );
        assert_eq!(second_value["hits"].as_array().map(Vec::len), Some(1));
        assert_ne!(
            first_value["hits"][0]["symbol"]["id"], second_value["hits"][0]["symbol"]["id"],
            "consecutive pages must serve distinct declarations"
        );
        Ok(())
    }

    #[test]
    fn get_symbol_page_past_the_end_is_empty_with_the_true_page_count() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "Beacon", "limit": 1, "page_index": 40}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        assert_eq!(value["hits"], json!([]));
        assert_eq!(
            value["pagination"],
            json!({ "page_index": 40, "total_pages": 2 })
        );
        Ok(())
    }

    /// The wire kind composes from the language name alone: a dialect
    /// separates identity segments, never the kind spelling.
    #[test]
    fn wire_kind_joins_the_language_name_and_kind_with_a_dot() {
        let rust = Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        assert_eq!(
            super::wire_kind(&rust, "function_item").0,
            "rust.function_item"
        );
        let dialect = Language {
            name: "typescript".to_owned(),
            dialect: Some("tsx".to_owned()),
        };
        assert_eq!(super::wire_kind(&dialect, "class").0, "typescript.class");
    }

    #[test]
    fn language_provider_routes_node_facets_through_the_registry() {
        let rust = Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        assert_eq!(
            super::language_provider(&rust).node_facets("function_item"),
            [NodeFacet::Declaration, NodeFacet::Definition]
        );
    }

    #[test]
    #[should_panic(expected = "must have a registered syntax provider: language=stub:mock")]
    fn language_provider_panics_naming_an_unregistered_language() {
        let stub = Language {
            name: "stub".to_owned(),
            dialect: Some("mock".to_owned()),
        };
        let _ = super::language_provider(&stub);
    }

    /// `projection` left `GetSymbolParams`'s served fields; a request naming it is refused as
    /// an unknown field, not accepted and silently ignored - whether or not `rev` rides
    /// alongside it.
    #[test]
    fn get_symbol_rejects_projection_as_an_unknown_field() {
        let cases = [
            json!({"name": "Beacon", "projection": "rift://projection/my-feature-one"}),
            json!({
                "name": "Beacon",
                "rev": "main",
                "projection": "rift://projection/my-feature-one"
            }),
        ];
        for case in cases {
            let result: Result<GetSymbolParams, _> = serde_json::from_value(case.clone());
            assert!(
                result.is_err(),
                "a withdrawn projection field must fail deserialization: {case}"
            );
        }
    }
}
