use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use rift_core::ProjectPath as CoreProjectPath;
use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_core::{
    Error, ErrorCode, ErrorContext, ErrorName, Fault, SourceVisibility, TextFileInclusion,
};
use rift_history::{HistoryError, Repository};
use rift_index::{
    FileDigest, IndexedFile, PathChanges, SymbolMatch, WorkspaceDigests, WorkspaceFingerprint,
    WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexLimits, WorkspaceSourcePolicy,
};
use rift_protocol::configuration::HistoryConfiguration;
use rift_protocol::read::{
    Digest, ExactKind, Extensions, FileId, GetSymbolHit, GetSymbolParams, GetSymbolResult,
    Language, Node, NodeFacet, NodeId, NodesParams, NodesResult, Pagination, ProjectPath,
    ReadWarning, RevisionId, SearchScope, SourceExcerpt, SourceKind, SourceLocation, SourceUnitId,
    SourceUnitSpan, Symbol, SymbolId, SymbolOrigin, TextRange,
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
    /// Request uses functionality this release does not serve.
    Unsupported {
        /// The unserved capability the request named.
        capability: String,
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
            Self::Unsupported { .. } => ErrorName::Wire(ErrorCode::CapabilityUnavailable),
            Self::Invalid { .. } => ErrorName::Wire(ErrorCode::InvalidRequest),
            Self::NotFound { .. } => ErrorName::Wire(ErrorCode::ResourceNotFound),
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
            Self::Invalid { field, violation } => vec![
                ErrorContext::new("field", *field),
                ErrorContext::new("violation", violation.clone()),
            ],
            Self::NotFound { path } => vec![ErrorContext::new("path", path.clone())],
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
            | Self::Invalid { .. }
            | Self::NotFound { .. }
            | Self::Storage { .. }
            | Self::Task { .. }
            | Self::Unavailable { .. }
            | Self::CapacityTimeout { .. } => None,
        }
    }
}

impl ReadFault {
    pub(crate) fn unsupported(capability: impl Into<String>) -> ReadError {
        Error::new(Self::Unsupported {
            capability: capability.into(),
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

    pub(crate) fn index(source: WorkspaceIndexError) -> ReadError {
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
        let span = tracing::info_span!(
            "index.build",
            component = "index",
            files_count = tracing::field::Empty,
            tree_revision = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let _entered = span.enter();
        let index =
            WorkspaceIndex::build(root, limits, visibility, text_inclusion).map_err(|source| {
                span.record("outcome", "error");
                ReadFault::index(source)
            })?;
        let source_policy = WorkspaceSourcePolicy::build(root, limits, visibility, text_inclusion)
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
        if let Some(violation) = rev.violation() {
            return Err(ReadFault::invalid("rev", violation.as_str()));
        }
        let repository = Repository::open(root).map_err(ReadFault::history)?;
        let resolved = repository.resolve(&rev.0).map_err(ReadFault::history)?;
        let index = WorkspaceIndex::at_revision(&repository, &resolved, limits, visibility)
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
    /// `stale_index` when the published index lags the captured tree, none
    /// when the two match.
    pub(crate) fn warnings(&self) -> Vec<ReadWarning> {
        self.revisions.warnings()
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
    /// symbol and included text file, chunked where a text file exceeds
    /// `[search.text].max_chunk`.
    #[must_use]
    pub fn lexical_units(&self) -> Vec<rift_index::LexicalUnit> {
        self.index.lexical_units()
    }

    /// Returns each included text file split into more than one lexical
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
    /// Returns [`ReadError`] for projections, invalid paths, or missing files.
    pub fn nodes(&self, params: NodesParams) -> Result<NodesResult, ReadError> {
        validate_common(
            params.projection.is_some(),
            params.rev.is_some(),
            SearchScope::Project,
        )?;
        let path = CoreProjectPath::new(params.path.0).map_err(|error| {
            ReadFault::invalid("path", rift_core::fault_label(&error.fault().violation()))
        })?;
        let file = self
            .index
            .file(&path)
            .ok_or_else(|| self.missing_file_fault(&path))?;
        let nodes = file
            .syntax()
            .nodes_at(params.position)
            .into_iter()
            .map(|node| wire_node(file, node))
            .collect();
        let source = vec![excerpt(
            file,
            ByteRange {
                start: params.position,
                end: params.position,
            },
        )];
        Ok(NodesResult {
            nodes,
            source,
            warnings: self.warnings(),
        })
    }

    /// The failure for a path the syntax index does not hold: `Unsupported` naming the
    /// unparsed extension when no shipped provider parses `path` and this snapshot can
    /// confirm the path is real; `not_found` for everything else, including a claimed
    /// extension the index simply has not read.
    fn missing_file_fault(&self, path: &CoreProjectPath) -> ReadError {
        match self.unclaimed_extension_capability(path) {
            Ok(Some(capability)) => ReadFault::unsupported(capability),
            Ok(None) => ReadFault::not_found(path.as_str()),
            Err(storage_fault) => storage_fault,
        }
    }

    /// `Some` naming `path`'s extension, or stating that it carries none, when no
    /// shipped syntax provider parses it and this snapshot can confirm the path is
    /// real. On the current tree that means the `[source]` policy makes `path` visible
    /// and the filesystem holds it; on a revision snapshot, which carries no filesystem
    /// policy, the extension alone decides, because `nodes` can never serve an
    /// unclaimed extension whatever tree it is pointed at.
    fn unclaimed_extension_capability(
        &self,
        path: &CoreProjectPath,
    ) -> Result<Option<String>, ReadError> {
        if extension_claimed(path) {
            return Ok(None);
        }
        let Some(policy) = self.source_policy.as_deref() else {
            return Ok(Some(extension_capability(path)));
        };
        let absolute = self.index.root().join(path.as_str());
        if !policy.visible(&absolute) {
            return Ok(None);
        }
        match absolute.try_exists() {
            Ok(true) => Ok(Some(extension_capability(path))),
            Ok(false) => Ok(None),
            Err(error) => Err(ReadFault::storage(path.as_str(), "stat", &error)),
        }
    }

    /// Finds declarations by name, with each hit's version-control timeline
    /// when the request asks for history.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for an unsupported projection or scope, and
    /// for symbol history the workspace's version control cannot serve.
    pub fn get_symbol(&self, params: &GetSymbolParams) -> Result<GetSymbolResult, ReadError> {
        validate_common(
            params.projection.is_some(),
            params.rev.is_some(),
            params.scope,
        )?;
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
                symbol: wire_symbol(matched),
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

pub(crate) fn validate_common(
    projection: bool,
    rev: bool,
    scope: SearchScope,
) -> Result<(), ReadError> {
    if rev && projection {
        return Err(ReadFault::invalid(
            "rev",
            "combines with projection; a read serves one tree",
        ));
    }
    if projection {
        return Err(ReadFault::unsupported("projection reads"));
    }
    if scope == SearchScope::Dependencies {
        return Err(ReadFault::unsupported("dependency reads"));
    }
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

pub(crate) fn wire_symbol(matched: SymbolMatch<'_>) -> Symbol {
    let symbol = matched.symbol;
    let language = matched.file.syntax().language();
    Symbol {
        id: symbol_id(matched.file, symbol),
        language: language.clone(),
        name: symbol.name.clone(),
        kind: wire_kind(language, symbol.kind),
        facets: symbol.facets.clone(),
        origin: SymbolOrigin {
            location: Some(SourceLocation::Project { package: None }),
            source_kind: SourceKind::Authored,
            unit: Some(source_unit_id(matched.file.path())),
        },
        container: symbol.container.as_ref().map(|container| {
            SymbolId(rift_core::symbol_identity(
                &language.identity_segment(),
                matched.file.path().as_str(),
                container,
            ))
        }),
        modifiers: Vec::new(),
        visibility: symbol.visibility.clone(),
        types: Vec::new(),
        signatures: Vec::new(),
        documentation: Vec::new(),
        extensions: Extensions(BTreeMap::new()),
        document_local: false,
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

/// Whether some shipped syntax provider parses `path`'s extension.
fn extension_claimed(path: &CoreProjectPath) -> bool {
    Path::new(path.as_str())
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| registry::provider_for_extension(extension).is_some())
}

/// The `Unsupported` capability text for a path no syntax provider parses: the
/// extension itself, or a statement that the path carries none.
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
pub(crate) fn node_witness(source: &str, range: ByteRange) -> String {
    let start = usize::try_from(range.start)
        .unwrap_or(source.len())
        .min(source.len());
    let end = usize::try_from(range.end)
        .unwrap_or(source.len())
        .min(source.len());
    digest_hex8(source.get(start..end).unwrap_or_default())
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

    use rift_core::SourceVisibility;
    use rift_protocol::read::{
        GetSymbolParams, Language, NodeFacet, NodesParams, NodesResult, ProjectPath, ProjectionId,
        RevisionId,
    };
    use serde_json::json;
    use tempfile::TempDir;

    use super::{HistoryConfiguration, ReadFault, ReadService, WorkspaceIndexLimits};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

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
            projection: None,
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
        assert_eq!(value["warnings"], json!([]));
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
    fn symbol_read_returns_source_and_stable_identity() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;

        assert_eq!(value["hits"][0]["symbol"]["name"], "Beacon");
        assert_eq!(value["hits"][0]["symbol"]["visibility"], "pub");
        assert!(
            value["hits"][0]["symbol"]["facets"]
                .as_array()
                .is_some_and(|facets| facets.contains(&json!("public")))
        );
        assert!(
            value["hits"][0]["symbol"]["id"]
                .as_str()
                .is_some_and(|id| id.contains("/Beacon"))
        );
        assert_eq!(value["hits"][0]["source"]["text"], "pub struct Beacon;");
        assert_eq!(
            value["hits"][0]["symbol"]["origin"]["unit"],
            json!("rift://source/project/src/lib.rs")
        );
        assert_eq!(
            value["pagination"],
            json!({ "page_index": 0, "total_pages": 1 })
        );
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

    #[test]
    fn unsupported_projection_is_rejected() -> TestResult {
        let (_directory, service) = fixture()?;
        let projection = ProjectionId("rift://projection/my-feature-one".to_owned());
        let nodes = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 0,
            projection: Some(projection),
            rev: None,
        });
        assert!(matches!(
            nodes.expect_err("projection must fail").fault(),
            ReadFault::Unsupported { .. }
        ));
        Ok(())
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
            projection: None,
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
            projection: None,
            rev: None,
        })
    }

    #[test]
    fn nodes_on_an_unparsed_path_names_its_extension() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("Cargo.lock"), "# generated\n")?;
        let service = nodes_service(directory.path(), &SourceVisibility::default())?;
        let error = nodes_at_root(&service, "Cargo.lock")
            .expect_err("an unparsed extension must be rejected");
        let ReadFault::Unsupported { capability } = error.fault() else {
            panic!("expected Unsupported, got {:?}", error.fault());
        };
        assert_eq!(capability, "lock files", "the refusal names the extension");
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
        let ReadFault::Unsupported { capability } = error.fault() else {
            panic!("expected Unsupported, got {:?}", error.fault());
        };
        assert_eq!(
            capability, "files with no extension",
            "justfile carries no extension at all"
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
            projection: None,
            rev: None,
        });
        assert!(matches!(
            result.expect_err("absolute path must fail").fault(),
            ReadFault::Invalid { .. }
        ));
        Ok(())
    }

    #[test]
    fn get_symbol_rejects_dependency_scope() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "Beacon", "scope": "dependencies"}))?;
        assert!(matches!(
            service
                .get_symbol(&params)
                .expect_err("dependency scope must fail")
                .fault(),
            ReadFault::Unsupported { .. }
        ));
        Ok(())
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
            projection: None,
            rev: None,
        })?;
        assert!(any_node_has_facet(&expression, "expression")?);

        let statement_position = RICH_SOURCE
            .find("total;")
            .ok_or("fixture must contain statement")? as u64;
        let statement = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: statement_position,
            projection: None,
            rev: None,
        })?;
        assert!(any_node_has_facet(&statement, "statement")?);

        let comment_position = RICH_SOURCE
            .find("lookout")
            .ok_or("fixture must contain comment")? as u64;
        let comment = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: comment_position,
            projection: None,
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
    fn nodes_backlink_documented_item_to_its_symbol() -> TestResult {
        let (_directory, service) = documented_fixture()?;
        let position = DOCUMENTED_SOURCE
            .find("Beacon")
            .ok_or("fixture must contain the struct name")? as u64;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position,
            projection: None,
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
            projection: None,
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
            projection: None,
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
            projection: None,
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
        assert_eq!(symbol["facets"], json!([]));
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
            projection: None,
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
        assert_eq!(symbol["facets"], json!([]));
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
        assert_eq!(symbol["facets"], json!([]));
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
            projection: None,
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
            projection: None,
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
    fn revision_nodes_on_an_unparsed_path_names_the_extension_without_a_policy() -> TestResult {
        let directory = committed_fixture()?;
        let service = revision_service(directory.path(), "main")?;
        let error = service
            .nodes(NodesParams {
                path: ProjectPath("Cargo.lock".to_owned()),
                position: 0,
                projection: None,
                rev: Some(RevisionId("main".to_owned())),
            })
            .expect_err("an unparsed extension must be rejected at a revision too");
        let ReadFault::Unsupported { capability } = error.fault() else {
            panic!("expected Unsupported, got {:?}", error.fault());
        };
        assert_eq!(
            capability, "lock files",
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
        assert_eq!(value["warnings"], json!([]));
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
            projection: None,
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

    #[test]
    fn rev_with_projection_is_refused_as_invalid() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({
            "name": "Beacon",
            "rev": "main",
            "projection": "rift://projection/my-feature-one"
        }))?;
        let error = service
            .get_symbol(&params)
            .expect_err("rev with projection must refuse before either is served");
        assert!(matches!(
            error.fault(),
            ReadFault::Invalid { field: "rev", .. }
        ));
        let nodes = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 0,
            projection: Some(ProjectionId("rift://projection/my-feature-one".to_owned())),
            rev: Some(RevisionId("main".to_owned())),
        });
        assert!(matches!(
            nodes.expect_err("rev with projection must refuse").fault(),
            ReadFault::Invalid { field: "rev", .. }
        ));
        Ok(())
    }
}
