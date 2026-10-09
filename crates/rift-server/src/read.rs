use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use rift_core::ProjectPath as CoreProjectPath;
use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion, line};
use rift_dependency::{DependencyContext, StandardLibrary};
pub(crate) use rift_error::RiftError;
use rift_error::errors;
use rift_history::Repository;
use rift_index::{
    FileDigest, FileRecord, IndexRead, IndexedFile, PathChange, PathChanges, ReadableSymbol,
    RelationshipStore, SymbolMatch, WorkspaceContentCache, WorkspaceDigests, WorkspaceFingerprint,
    WorkspaceIndex, WorkspaceIndexLimits, WorkspaceIndexPreparation, WorkspaceIndexWarning,
    WorkspaceSourcePolicy,
};
use rift_protocol::configuration::HistoryConfiguration;
use rift_protocol::dependencies::{
    DependenciesConfiguration, DependencyResolution, REQUESTED_PACKAGES_MAX, RequestedPackage,
};
use rift_protocol::map::WorkspaceMap;
use rift_protocol::read::{
    Digest, ExactKind, Extensions, FileId, GetSymbolHit, GetSymbolInclude, GetSymbolParams,
    GetSymbolResult, Language, Node, NodeFacet, NodeId, NodesParams, NodesResult, PAGE_LIMIT_MAX,
    Pagination, ProjectPath, ReadWarning, RevisionId, SOURCE_WARNINGS_MAX, SearchScope,
    SourceLocationKind, SourceUnitId, Symbol, SymbolId, TextRange,
};
use rift_syntax::{ByteRange, SyntaxNode, SyntaxProvider, SyntaxSymbol, registry};
use sha2::{Digest as _, Sha256};

use crate::history::{StoredHistory, SymbolTimelines};

/// Inputs for one cancellable current-tree read-service build.
pub struct ReadServiceBuild<'a> {
    /// Workspace root to index.
    pub root: &'a Path,
    /// Limits applied while indexing and reading.
    pub limits: WorkspaceIndexLimits,
    /// Accepted path visibility policy.
    pub visibility: &'a SourceVisibility,
    /// Accepted text and documentation file selection.
    pub text_inclusion: &'a TextFileInclusion,
    /// Accepted language path selections.
    pub languages: &'a LanguageFileSelections,
    /// History behavior served from this snapshot.
    pub history: HistoryConfiguration,
    /// Dependency context inputs and version probe behavior.
    pub dependencies: DependenciesConfiguration,
    /// Returns true when indexing work should stop.
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
    /// Shared source and syntax facts cache for workspace builds.
    pub content_cache: Option<&'a WorkspaceContentCache>,
}

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
    /// The packages the workspace's manifests, lockfiles, and languages name, read from
    /// those files and shared across incremental rebuilds until one of them changes.
    /// Empty for a revision snapshot, which has no working tree to read.
    context: Arc<DependencyContext>,
    /// The accepted `[dependencies]` table: the configured packages, and whether the
    /// standard library version probes run.
    dependency_configuration: DependenciesConfiguration,
    /// The history store symbol-history reads answer from, attached by the serving
    /// layer; with none attached, a timeline walks git per request.
    stored_history: OnceLock<StoredHistory>,
}

impl ReadService {
    /// Builds one in-memory snapshot from real workspace files, applying
    /// `visibility`'s `.gitignore` and `[source]` policy on top of the hard
    /// floor. `history` gates and bounds later symbol-history reads served
    /// from this snapshot. The dependency context reads the static inputs alone,
    /// as `[dependencies] resolution = "static"` does, so no version probe runs and
    /// the snapshot answers the same on every machine.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when root cannot be indexed within bounds.
    pub fn build(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        history: HistoryConfiguration,
    ) -> Result<Self, RiftError> {
        Self::build_with_languages(
            root,
            limits,
            visibility,
            text_inclusion,
            &LanguageFileSelections::default(),
            history,
            DependenciesConfiguration {
                resolution: DependencyResolution::Static,
                ..DependenciesConfiguration::default()
            },
        )
    }

    /// Builds one current-tree snapshot with configured language entries.
    ///
    /// `dependencies` names the configured packages, whether the standard library
    /// version probes run, and how long one probe may take.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when configuration or root cannot be indexed
    /// within bounds.
    pub fn build_with_languages(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
        history: HistoryConfiguration,
        dependencies: DependenciesConfiguration,
    ) -> Result<Self, RiftError> {
        let cancelled = || false;
        Self::build_with_languages_cancellable(ReadServiceBuild {
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
            history,
            dependencies,
            cancelled: &cancelled,
            content_cache: None,
        })
    }

    /// Builds one current-tree snapshot, checking `cancelled` between files.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when configuration or root cannot be indexed within bounds,
    /// or when indexing is cancelled.
    pub fn build_with_languages_cancellable(
        build: ReadServiceBuild<'_>,
    ) -> Result<Self, RiftError> {
        let ReadServiceBuild {
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
            history,
            dependencies,
            cancelled,
            content_cache,
        } = build;
        let span = rift_tracing::info_span!(
            "index.build",
            component = "index",
            files_count = rift_tracing::empty!(),
            text_files_count = rift_tracing::empty!(),
            left_out_count = rift_tracing::empty!(),
            tree_revision = rift_tracing::empty!(),
            outcome = rift_tracing::empty!(),
        );
        span.in_scope(|| -> Result<Self, RiftError> {
            let cache = content_cache.cloned().unwrap_or_default();
            let index = WorkspaceIndex::build_with_languages_cancellable_and_cache(
                root,
                limits,
                visibility,
                text_inclusion,
                languages,
                &cache,
                cancelled,
            )
            .inspect_err(|_| {
                span.record("outcome", "error");
            })?;
            let source_policy = WorkspaceSourcePolicy::build_with_languages_cancellable(
                root,
                limits,
                visibility,
                text_inclusion,
                languages,
                cancelled,
            )
            .inspect_err(|_| {
                span.record("outcome", "error");
            })?;
            let revisions = captured_revisions(&index);
            let context = Arc::new(resolved_context(
                root,
                &source_policy,
                &dependencies,
                cancelled,
            )?);
            span.record("files_count", index.file_count());
            span.record("text_files_count", index.text_file_count());
            span.record("left_out_count", index.left_out_file_count());
            span.record("tree_revision", revisions.wire_tree_revision());
            span.record("outcome", "ok");
            Ok(Self {
                index,
                revisions,
                revision: None,
                history,
                source_policy: Some(Arc::new(source_policy)),
                context,
                dependency_configuration: dependencies,
                stored_history: OnceLock::new(),
            })
        })
    }

    /// Serves one prepared immutable index without walking or reading the workspace again.
    ///
    /// The caller supplies the source policy and dependency context captured for the same
    /// selected file set. Partial publications use an empty dependency context until their
    /// full selected set is prepared.
    #[must_use]
    pub fn from_prepared_index(
        index: WorkspaceIndex,
        source_policy: Option<Arc<WorkspaceSourcePolicy>>,
        context: Arc<DependencyContext>,
        history: HistoryConfiguration,
        dependencies: DependenciesConfiguration,
    ) -> Self {
        let revisions = captured_revisions(&index);
        Self {
            index,
            revisions,
            revision: None,
            history,
            source_policy,
            context,
            dependency_configuration: dependencies,
            stored_history: OnceLock::new(),
        }
    }

    /// Reads dependency context through one compiled source policy, checking `cancelled`
    /// before each version probe and while each one runs.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] if the policy cannot list visible paths, dependency
    /// resolution refuses its configured input, or the read is cancelled; a cancelled read
    /// answers no context.
    pub fn dependency_context_for_policy(
        root: &Path,
        source_policy: &WorkspaceSourcePolicy,
        dependencies: &DependenciesConfiguration,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<DependencyContext, RiftError> {
        resolved_context(root, source_policy, dependencies, cancelled)
    }

    /// Every file's digest this snapshot indexed, in project-path order.
    ///
    /// A request that captured the tree itself compares its capture with this to name the
    /// files that moved, instead of asking for the whole workspace.
    #[must_use]
    pub fn workspace_digests(&self) -> WorkspaceDigests {
        self.index.digests()
    }

    /// Effective bounds this index was built under.
    #[must_use]
    pub const fn workspace_limits(&self) -> WorkspaceIndexLimits {
        self.index.limits()
    }

    /// Every file's content digest this snapshot indexed, the files it left out included,
    /// in project-path order, without hashing any file again.
    #[must_use]
    pub fn content_digests(&self) -> WorkspaceDigests {
        self.index.content_digests()
    }

    /// Workspace orientation snapshot: language totals, the directory tree indexed files sit
    /// under, the most-referenced symbols, entry points, and docs - computed once from this
    /// snapshot's already-loaded index.
    #[must_use]
    pub fn workspace_map(&self) -> WorkspaceMap {
        crate::map::build_workspace_map(
            &self.index,
            &self.context,
            self.revisions.wire_index_tree_revision(),
        )
    }

    /// Builds file counts for paths already selected by workspace discovery. Symbol facts
    /// and dependency entries remain empty until the prepared index is complete.
    #[must_use]
    pub fn workspace_preparation_map(
        &self,
        source: &[(CoreProjectPath, Language)],
        text: &[CoreProjectPath],
    ) -> WorkspaceMap {
        crate::map::build_preparation_map(source, text, self.revisions.wire_index_tree_revision())
    }

    /// The symbol reference adjacency built from this snapshot's normalized graph: which
    /// symbols reference which, in both directions.
    #[must_use]
    pub fn relationships(&self) -> &RelationshipStore {
        self.index.relationships()
    }

    /// The visibility policy this snapshot reads the filesystem through.
    ///
    /// A revision snapshot answers from version control alone and carries none,
    /// so every capture that touches the tree refuses here first, naming the
    /// operation the caller asked for.
    fn filesystem_policy(
        &self,
        operation: &'static str,
    ) -> Result<&WorkspaceSourcePolicy, RiftError> {
        self.source_policy.as_deref().ok_or_else(|| {
            errors::server::read_task()
                .operation(operation)
                .detail("a revision snapshot has no filesystem tree")
                .error()
        })
    }

    /// Returns every visible regular file's content digest.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when this is a revision snapshot or capture fails.
    pub fn visible_workspace_digests(&self) -> Result<WorkspaceDigests, RiftError> {
        self.filesystem_policy("capture visible workspace digests")?
            .visible_digests()
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

    /// What this snapshot records at `path`, as a change set compares it: the digest of
    /// the bytes it read, or the warning naming a file it left out before reading one.
    #[must_use]
    pub fn file_record(&self, path: &CoreProjectPath) -> Option<FileRecord> {
        self.index.record(path)
    }

    /// The exact warning this snapshot holds for a file it left out, before wire warning
    /// limits are applied. Index construction keeps these warnings in project-path order.
    #[must_use]
    pub fn file_warning(&self, path: &CoreProjectPath) -> Option<&WorkspaceIndexWarning> {
        let warnings = self.index.warnings();
        warnings
            .binary_search_by(|warning| warning.path().cmp(path))
            .ok()
            .map(|position| &warnings[position])
    }

    /// Whether this snapshot holds at least one file below `directory`, the files it
    /// left out included.
    ///
    /// A filesystem event names a path, and whether that path is a directory the index
    /// holds files under decides whether one file or the whole workspace is read again.
    #[must_use]
    pub fn holds_files_below(&self, directory: &CoreProjectPath) -> bool {
        self.index.holds_files_below(directory)
    }

    /// Builds the next snapshot from a whole scan of the tree, sharing every file whose bytes
    /// this snapshot already parsed.
    ///
    /// The source policy is compiled again, so a rewritten ignore file decides what is
    /// visible, and the dependency context is read again from the manifests and lockfiles
    /// that policy makes visible. The language entries, bounds, text selection, history, and
    /// dependency configuration carry over, which is why a caller rescans only while the
    /// index-owned configuration is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the root, the policy, or a visible file cannot be indexed
    /// within bounds.
    pub fn rescanned(
        &self,
        root: &Path,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
    ) -> Result<Self, RiftError> {
        self.rescanned_cancellable(root, visibility, text_inclusion, languages, &|| false)
    }

    /// Rescans one current-tree snapshot, checking `cancelled` between files.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the root, policy, or a visible file cannot be indexed
    /// within bounds, or when indexing is cancelled.
    pub fn rescanned_cancellable(
        &self,
        root: &Path,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let span = rift_tracing::info_span!(
            "index.build",
            component = "index",
            mode = "rescan",
            files_count = rift_tracing::empty!(),
            tree_revision = rift_tracing::empty!(),
            outcome = rift_tracing::empty!(),
        );
        span.in_scope(|| -> Result<Self, RiftError> {
            let built = self
                .index
                .rescanned_cancellable(visibility, cancelled)
                .and_then(|index| {
                    let source_policy = WorkspaceSourcePolicy::build_with_languages_cancellable(
                        root,
                        self.index.limits(),
                        visibility,
                        text_inclusion,
                        languages,
                        cancelled,
                    )?;
                    Ok((index, source_policy))
                });
            let (index, source_policy) = built.inspect_err(|_| {
                span.record("outcome", "error");
            })?;
            let revisions = captured_revisions(&index);
            let context = Arc::new(resolved_context(
                root,
                &source_policy,
                &self.dependency_configuration,
                cancelled,
            )?);
            span.record("files_count", index.file_count());
            span.record("tree_revision", revisions.wire_tree_revision());
            span.record("outcome", "ok");
            Ok(Self {
                index,
                revisions,
                revision: self.revision.clone(),
                history: self.history.clone(),
                source_policy: Some(Arc::new(source_policy)),
                context,
                dependency_configuration: self.dependency_configuration.clone(),
                stored_history: self.carried_history(),
            })
        })
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
    /// Returns [`RiftError`] when this is a revision snapshot, which has no filesystem tree
    /// to read the named paths from, or when a named path cannot be read or indexed within
    /// bounds.
    pub fn rebuilt(&self, changes: &PathChanges) -> Result<Self, RiftError> {
        self.rebuilt_cancellable(changes, &|| false)
    }

    /// Rebuilds the named paths, checking `cancelled` between files.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a named path cannot be indexed within bounds, or when
    /// indexing is cancelled.
    pub fn rebuilt_cancellable(
        &self,
        changes: &PathChanges,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let source_policy = self.filesystem_policy("incremental rebuild")?;
        let span = rift_tracing::info_span!(
            "index.build",
            component = "index",
            changed_count = changes.len(),
            files_count = rift_tracing::empty!(),
            tree_revision = rift_tracing::empty!(),
            outcome = rift_tracing::empty!(),
        );
        span.in_scope(|| -> Result<Self, RiftError> {
            let index = self
                .index
                .rebuilt_cancellable(changes, cancelled)
                .inspect_err(|_| {
                    span.record("outcome", "error");
                })?;
            let revisions = captured_revisions(&index);
            let context = self.context_after(source_policy, changes, &index, cancelled)?;
            span.record("files_count", index.file_count());
            span.record("tree_revision", revisions.wire_tree_revision());
            span.record("outcome", "ok");
            Ok(Self {
                index,
                revisions,
                revision: self.revision.clone(),
                history: self.history.clone(),
                source_policy: self.source_policy.clone(),
                context,
                dependency_configuration: self.dependency_configuration.clone(),
                stored_history: self.carried_history(),
            })
        })
    }

    /// The dependency context an incremental rebuild over `changes` carries: the standing
    /// one while no changed path is one of its inputs or a manifest a resolver claims, no
    /// change adds a language's first visible path or removes its last, and no project
    /// environment it listed lists differently now; a fresh read otherwise. Reading it
    /// costs one pass over the workspace's manifests and lockfiles, so a rebuild that
    /// touches none of them keeps what it holds.
    ///
    /// `index` is the rebuilt index: a removed path's language counts as gone only when
    /// no file it still holds is of that language.
    fn context_after(
        &self,
        source_policy: &WorkspaceSourcePolicy,
        changes: &PathChanges,
        index: &WorkspaceIndex,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Arc<DependencyContext>, RiftError> {
        let touches_input = changes.paths().any(|path| {
            let path = project_path(path);
            self.context.depends_on(&path) || rift_dependency::is_claimed_manifest(&path)
        });
        if !touches_input
            && !self.changes_standard_libraries(source_policy, changes, index)
            && !self.project_environment_moved()
        {
            return Ok(Arc::clone(&self.context));
        }
        let context = resolved_context(
            self.index.root(),
            source_policy,
            &self.dependency_configuration,
            cancelled,
        )?;
        Ok(Arc::new(context))
    }

    /// Whether the dependency context listed a project environment, so
    /// [`Self::project_environment_moved`] has one to observe.
    #[must_use]
    pub fn observes_project_environment(&self) -> bool {
        self.context.observes_project_environment()
    }

    /// Whether a project environment the dependency context listed lists differently now:
    /// a distribution `uv sync` installed, removed, or upgraded while the lockfile stood
    /// still. The environment sits outside every visible path, so no changed path reports
    /// it, and a context that listed no environment answers `false` without a read.
    #[must_use]
    pub fn project_environment_moved(&self) -> bool {
        self.context.observes_project_environment()
            && self.context.project_environment_moved(
                &mut crate::dependency::FilesystemInputs::new(
                    crate::dependency::ResolutionPolicy::from(&self.dependency_configuration),
                ),
            )
    }

    /// Whether `changes` adds the first visible path of a language whose standard
    /// library the standing context does not name, or removes the last path of one it
    /// does. A Rust workspace gaining `scripts/tool.py` then names `stdlib/python` on
    /// the next read.
    fn changes_standard_libraries(
        &self,
        source_policy: &WorkspaceSourcePolicy,
        changes: &PathChanges,
        index: &WorkspaceIndex,
    ) -> bool {
        let named = self.context.standard_libraries();
        changes.iter().any(|(path, change)| {
            let Some(library) = path_library(source_policy, path.as_str()) else {
                return false;
            };
            match change {
                PathChange::Added => !named.contains(&library),
                PathChange::Removed => {
                    named.contains(&library)
                        && !index.files().any(|file| {
                            path_library(source_policy, file.path().as_str()) == Some(library)
                        })
                }
                PathChange::Modified => false,
            }
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
    /// Returns [`RiftError`] when the spelling breaks the advertised
    /// charset, the workspace has no repository, the revision does not
    /// resolve to a commit, or the revision tree cannot be indexed within
    /// bounds.
    pub fn at_revision(
        root: &Path,
        rev: &RevisionId,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        history: HistoryConfiguration,
    ) -> Result<Self, RiftError> {
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
    /// Returns [`RiftError`] when revision or configuration cannot be served.
    pub fn at_revision_with_languages(
        root: &Path,
        rev: &RevisionId,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        text_inclusion: &TextFileInclusion,
        languages: &LanguageFileSelections,
        history: HistoryConfiguration,
    ) -> Result<Self, RiftError> {
        if let Some(violation) = rev.violation() {
            return errors::server::read_invalid()
                .field("rev")
                .violation(violation.as_str())
                .fail();
        }
        let repository = Repository::open(root)?;
        let resolved = repository.resolve(&rev.0)?;
        let limits = limits.with_revision_tree_entries(
            usize::try_from(history.tree_entries).unwrap_or(usize::MAX),
        )?;
        let index = WorkspaceIndex::at_revision_with_languages(
            &repository,
            &resolved,
            limits,
            visibility,
            text_inclusion,
            languages,
        )?;
        let revisions = captured_revisions(&index);
        Ok(Self {
            index,
            revisions,
            revision: Some(RevisionId(resolved.commit_id())),
            history,
            source_policy: None,
            context: Arc::new(DependencyContext::default()),
            dependency_configuration: DependenciesConfiguration::default(),
            stored_history: OnceLock::new(),
        })
    }

    /// Returns the immutable workspace index this snapshot serves.
    pub(crate) const fn index(&self) -> &WorkspaceIndex {
        &self.index
    }

    /// Advances local workspace preparation, reusing derived data from this snapshot when
    /// its accepted workspace inputs match.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an invalid target, read, syntax, bound, or
    /// cancellation.
    ///
    /// # Panics
    ///
    /// Panics before discovery or when `target` moves backward or exceeds selected files.
    pub fn advance_workspace_preparation(
        &self,
        preparation: &mut WorkspaceIndexPreparation,
        target: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<WorkspaceIndex, RiftError> {
        preparation.advance_to_with_previous(target, Some(&self.index), cancelled)
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

    /// Attaches the history store symbol-history reads answer from. The serving layer
    /// opens one store and attaches it to every snapshot it serves; the first attach
    /// stays, and an incremental rebuild carries it forward.
    pub fn attach_history_store(&self, store: &StoredHistory) {
        // The cell holds the one store the server opened, so a second attach of the same
        // store has nothing to replace.
        let _attached = self.stored_history.get_or_init(|| store.clone());
    }

    /// The history store the serving layer attached, which symbol-history reads and
    /// commit searches answer from.
    pub(crate) fn stored_history(&self) -> Option<&StoredHistory> {
        self.stored_history.get()
    }

    /// The accepted `[providers.history]` table this snapshot serves under.
    pub(crate) const fn history_configuration(&self) -> &HistoryConfiguration {
        &self.history
    }

    /// The attached history store, for a snapshot built from this one.
    fn carried_history(&self) -> OnceLock<StoredHistory> {
        self.stored_history
            .get()
            .map_or_else(OnceLock::new, |store| OnceLock::from(store.clone()))
    }

    /// The packages the workspace's manifests and lockfiles name, as this snapshot read
    /// them.
    #[must_use]
    pub const fn dependency_context(&self) -> &Arc<DependencyContext> {
        &self.context
    }

    /// The dependency context one current-tree read resolves through the global API:
    /// this snapshot's own, with the request's `packages` applied through
    /// [`DependencyContext::with_requested`] under the compiled entry bound
    /// [`rift_dependency::PACKAGES_MAX`]. A request naming no package reads the snapshot's
    /// context as it is. A caller that learns a smaller bound from the global API applies
    /// the same packages to [`Self::dependency_context`] under it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] naming `packages` for an argument the read cannot send:
    /// beside the `local` scope, beside `rev`, past `REQUESTED_PACKAGES_MAX` entries, or
    /// with an entry outside its advertised lengths.
    pub fn read_context(
        &self,
        scope: SearchScope,
        rev: Option<&RevisionId>,
        packages: &[RequestedPackage],
    ) -> Result<Arc<DependencyContext>, RiftError> {
        validate_requested_packages(scope, rev.is_some(), packages)?;
        if packages.is_empty() {
            return Ok(Arc::clone(&self.context));
        }
        Ok(Arc::new(
            self.context
                .with_requested(packages, rift_dependency::PACKAGES_MAX),
        ))
    }

    /// The accepted `[dependencies]` table this snapshot was built under.
    #[must_use]
    pub const fn dependency_configuration(&self) -> &DependenciesConfiguration {
        &self.dependency_configuration
    }

    /// Returns the warnings every answer from this service carries: one
    /// `stale_index` when the published index lags the captured tree, one
    /// `source_unavailable` for each file the index left out, bounded by
    /// [`SOURCE_WARNINGS_MAX`], and none of either when nothing applies.
    pub(crate) fn warnings(&self) -> Vec<ReadWarning> {
        let mut warnings = self.revisions.warnings();
        warnings.extend(source_warnings(self.index.warnings()));
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
    pub fn index_documents(&self) -> Vec<rift_ranking::IndexDocument> {
        self.index.index_documents()
    }

    /// Symbol documents grouped by file for vector population.
    #[must_use]
    pub fn symbol_index_documents_by_file(&self) -> Vec<Arc<[rift_ranking::IndexDocument]>> {
        self.index.symbol_index_documents_by_file()
    }

    /// Shares this captured tree's validated documentation metadata for atomic publication.
    #[must_use]
    pub fn documentation_snapshot(&self) -> Arc<rift_index::DocumentationCollection> {
        self.index.documentation_snapshot()
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
    /// Returns [`RiftError`] for invalid paths, missing files, or a `position` at or past the
    /// file's byte length.
    pub fn nodes(&self, params: NodesParams) -> Result<NodesResult, RiftError> {
        validate_common(params.rev.is_some())?;
        let path = CoreProjectPath::new(params.path.0).map_err(|error| {
            errors::server::read_invalid()
                .field("path")
                .violation(error.detail())
                .cause(error)
                .error()
        })?;
        let file = self
            .index
            .file(&path)
            .ok_or_else(|| self.missing_file_fault(&path))?;
        validate_node_position(file, params.position)?;
        let nodes = self
            .index
            .nodes(&path, params.position)?
            .ok_or_else(|| self.missing_file_fault(&path))?;
        Ok(nodes_at_file(file, &nodes, self.revisions.warnings()))
    }

    /// Reads syntax nodes for a path selected by workspace discovery but not yet included in
    /// this partial index. The captured source policy validates the path before and after the
    /// bounded parse, and the parsed digest must match both reads.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an invalid or absent path, a path outside the selected source
    /// set, a parse refusal, or a source that changed during the read.
    pub fn nodes_for_preparation(&self, params: NodesParams) -> Result<NodesResult, RiftError> {
        validate_common(params.rev.is_some())?;
        let path = CoreProjectPath::new(params.path.0.clone()).map_err(|error| {
            errors::server::read_invalid()
                .field("path")
                .violation(error.detail())
                .cause(error)
                .error()
        })?;
        if self.file_record(&path).is_some() {
            return self.nodes(params);
        }
        let Some(policy) = self.source_policy.as_deref() else {
            return errors::server::read_unavailable()
                .operation("nodes")
                .detail("workspace discovery has not selected a source path")
                .fail();
        };
        let absolute = self.index.root().join(path.as_str());
        let before = policy.visible_digest(&absolute).map_err(|error| {
            if error.slug() == errors::index::workspace_file_too_large::SLUG
                || self.index_names_omission(&path)
            {
                errors::server::read_source_unavailable()
                    .path(path.as_str())
                    .error()
            } else {
                error
            }
        })?;
        let Some(before) = before else {
            return self.nodes(params);
        };
        let Some(parsed) = self
            .index
            .parse_source_file_with_nodes(&path, params.position)?
        else {
            return self.nodes(params);
        };
        let parsed = match parsed {
            IndexRead::Included(parsed) => parsed,
            IndexRead::Skipped(_) => {
                return errors::server::read_source_unavailable()
                    .path(path.as_str())
                    .fail();
            }
        };
        let after = policy.visible_digest(&absolute).map_err(|error| {
            if error.slug() == errors::index::workspace_file_too_large::SLUG
                || self.index_names_omission(&path)
            {
                errors::server::read_unavailable()
                    .operation("nodes")
                    .detail("source changed during the targeted read")
                    .error()
            } else {
                error
            }
        })?;
        if before != parsed.file.digest() || after != Some(before) {
            return errors::server::read_unavailable()
                .operation("nodes")
                .detail("source changed during the targeted read")
                .fail();
        }
        validate_node_position(&parsed.file, params.position)?;
        Ok(nodes_at_file(
            &parsed.file,
            &parsed.nodes,
            self.revisions.warnings(),
        ))
    }

    /// The failure for a path the syntax index does not hold: `content_unavailable` when
    /// this snapshot's warnings name an omission, the capability a syntax read lacks when
    /// this snapshot can confirm the path is real, and `not_found` otherwise.
    fn missing_file_fault(&self, path: &CoreProjectPath) -> RiftError {
        if self.index_names_omission(path) {
            return errors::server::read_source_unavailable()
                .path(path.as_str())
                .error();
        }
        match self.unserved_syntax(path) {
            Ok(Some(UnservedSyntax::Configurable(capability))) => {
                errors::server::read_unsupported()
                    .capability(capability)
                    .error()
            }
            Ok(Some(UnservedSyntax::Unclaimed(extension))) => {
                errors::server::read_unclaimed_extension()
                    .extension(extension)
                    .error()
            }
            Ok(None) => errors::server::read_not_found().path(path.as_str()).error(),
            Err(storage_fault) => storage_fault,
        }
    }

    /// Whether this snapshot's own warnings name `path` as left out of the index.
    fn index_names_omission(&self, path: &CoreProjectPath) -> bool {
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
    /// Returns [`RiftError`] when two language entries match `path`, or when the
    /// filesystem cannot answer whether it exists.
    fn unserved_syntax(&self, path: &CoreProjectPath) -> Result<Option<UnservedSyntax>, RiftError> {
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
    ) -> Result<Option<UnservedSyntax>, RiftError> {
        let claim = self
            .index
            .language_policy()
            .language_for_path(Path::new(path.as_str()))?;
        Ok(match claim {
            Some(language) if language.enabled() && language.has_syntax() => None,
            Some(language) => Some(UnservedSyntax::Configurable(format!(
                "{} files",
                language.identity()
            ))),
            None => Some(UnservedSyntax::Unclaimed(extension_capability(path))),
        })
    }

    /// Whether this snapshot can confirm `path` names a real visible file. On the
    /// current tree the `[source]` policy has to make `path` visible and the
    /// filesystem has to hold it; a revision snapshot carries no filesystem policy,
    /// so the tree it was built from is the answer.
    fn path_is_real(&self, path: &CoreProjectPath) -> Result<bool, RiftError> {
        let Some(policy) = self.source_policy.as_deref() else {
            return Ok(true);
        };
        let absolute = self.index.root().join(path.as_str());
        if !policy.visible(&absolute) {
            return Ok(false);
        }
        absolute.try_exists().map_err(|error| {
            errors::server::read_storage()
                .path(path.as_str())
                .operation("stat")
                .io(&error)
                .error()
        })
    }

    /// Finds declarations by name, with each hit's version-control timeline
    /// when the request asks for history. The project index answers `local` and
    /// `all`; package facts come from the global index, so a `global` lookup
    /// answers no project hit. A scope beyond `local` refuses `rev`, since package
    /// facts are served for the current tree alone.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for a scope beyond `local` beside `rev`, a `packages`
    /// argument the read cannot send, and symbol history the workspace's version control
    /// cannot serve.
    pub fn get_symbol(&self, params: &GetSymbolParams) -> Result<GetSymbolResult, RiftError> {
        validate_common(params.rev.is_some())?;
        accepted_symbol_name(&params.name)?;
        let limit = accepted_limit(params.limit)?;
        validate_requested_packages(params.scope, params.rev.is_some(), &params.packages)?;
        self.validate_dependency_scope(params.scope, params.rev.as_ref())?;
        // The whole ranked match set is collected up to the project index's own
        // `results_max` bound, so `pagination.total_pages` counts the full result set the
        // pages divide.
        let results_max = self.index.results_max();
        let mut candidates = if params.scope == SearchScope::Global {
            Vec::new()
        } else {
            self.index.symbols(&params.name, results_max)?
        };
        let bound_reached = candidates.len() >= results_max;
        if let Some(language) = &params.language {
            candidates
                .retain(|matched| language_selects(language, matched.file.syntax().language()));
        }
        let absent = candidates.is_empty();
        let (window, pagination) = page(candidates, params.page_index, limit);
        let include_source = params.include.contains(&GetSymbolInclude::Source);
        let include_history = params.include.contains(&GetSymbolInclude::History);
        let include_documentation = params.include.contains(&GetSymbolInclude::Documentation);
        let mut documentation_bytes_left =
            rift_protocol::documentation::DOCUMENTATION_EXCERPT_BYTES_MAX as usize;
        let mut timelines = if include_history {
            Some(self.symbol_timelines()?)
        } else {
            None
        };
        let mut hits = Vec::with_capacity(window.len());
        let mut disagreements = Vec::new();
        for matched in window {
            let (mut hit, disagreement) =
                self.project_hit(matched, include_source, timelines.as_mut())?;
            disagreements.extend(disagreement);
            if include_documentation && let Some(symbol) = &hit.symbol.id {
                hit.documentation = Some(rift_index::documentation_context_with_budget(
                    self.index.documentation(),
                    symbol,
                    |source| self.index.documentation_content(source),
                    &mut documentation_bytes_left,
                ));
            }
            hits.push(hit);
        }
        let mut warnings = self.warnings();
        warnings.extend(disagreements);
        if absent {
            warnings.push(self.symbol_not_found(params));
        }
        if bound_reached {
            warnings.push(results_truncation_warning(results_max));
        }
        Ok(GetSymbolResult {
            hits,
            pagination,
            warnings,
        })
    }

    /// Names an absent lookup and complete alternatives, or their work-bound failure.
    fn symbol_not_found(&self, params: &GetSymbolParams) -> ReadWarning {
        let proposed = if params.scope == SearchScope::Global {
            Ok(Vec::new())
        } else {
            crate::alternatives::symbols(self.index.files(), &params.name, params.language.as_ref())
        };
        let (alternatives, detail) = match proposed {
            Ok(alternatives) => (alternatives, None),
            Err(()) => (
                Vec::new(),
                Some(crate::alternatives::UNAVAILABLE_DETAIL.to_owned()),
            ),
        };
        ReadWarning::SymbolNotFound {
            name: params.name.clone(),
            alternatives,
            detail,
        }
    }

    /// Refuses a `scope` that reaches packages on a revision read - one the request's
    /// `rev` names, or the revision this snapshot already serves - since package facts
    /// are served for the current tree alone. `get_symbol` and `search` share the rule.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] naming `scope` for a scope beyond `local` beside a
    /// revision.
    pub(crate) fn validate_dependency_scope(
        &self,
        scope: SearchScope,
        rev: Option<&RevisionId>,
    ) -> Result<(), RiftError> {
        if scope == SearchScope::Local {
            return Ok(());
        }
        if rev.is_some() || self.revision.is_some() {
            return errors::server::read_invalid()
                .field("scope")
                .violation(CURRENT_TREE_ALONE)
                .fail();
        }
        Ok(())
    }

    /// One `get_symbol` hit from the project index, with the `symbol_disagreement`
    /// warning its assembly raised.
    fn project_hit(
        &self,
        matched: SymbolMatch<'_>,
        include_source: bool,
        timelines: Option<&mut SymbolTimelines>,
    ) -> Result<(GetSymbolHit, Option<ReadWarning>), RiftError> {
        let history = match timelines {
            Some(timelines) => Some(
                timelines.timeline(language_provider(matched.file.syntax().language()), matched)?,
            ),
            None => None,
        };
        let (symbol, disagreement) = wire_symbol(&self.index, matched)?;
        let (path, unit) = hit_location(symbol.origin.location, matched.file.path());
        let hit = GetSymbolHit {
            symbol,
            path,
            unit,
            range: text_range(matched.symbol.range),
            line: line::line_number_at(matched.file.source(), matched.symbol.range.start),
            node: include_source.then(|| symbol_node(matched).id),
            source: include_source.then(|| excerpt(matched.file, matched.symbol.range)),
            history,
            documentation: None,
        };
        Ok((hit, disagreement))
    }

    /// Opens this snapshot's timeline composition, starting at the served
    /// commit for a revision read and at `HEAD` for a current-tree read, and
    /// reading the attached history store when there is one.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `[providers.history]` is disabled, the
    /// workspace's version control cannot serve a start, or the attached
    /// store cannot be read.
    fn symbol_timelines(&self) -> Result<SymbolTimelines, RiftError> {
        SymbolTimelines::open(
            self.index.root(),
            self.revision.as_ref(),
            &self.history,
            self.index.limits().syntax(),
            self.stored_history.get(),
        )
    }
}

/// Accepts a nonempty lookup name within the served character bound.
fn accepted_symbol_name(name: &str) -> Result<(), RiftError> {
    let maximum = rift_protocol::read::SYMBOL_NAME_CHARACTERS_MAX;
    let characters = name.chars().take(maximum + 1).count();
    if characters == 0 || characters > maximum {
        return errors::server::read_invalid()
            .field("name")
            .violation(format!("expected 1 to {maximum} characters"))
            .fail();
    }
    Ok(())
}

/// Accepts a caller-supplied page size within the served limit.
///
/// # Errors
///
/// Returns `invalid_request` naming `limit` when the size is outside its bounds.
pub fn accepted_limit(requested: u64) -> Result<usize, RiftError> {
    if requested == 0 {
        return errors::server::read_invalid()
            .field("limit")
            .violation("zero")
            .fail();
    }
    if requested > PAGE_LIMIT_MAX {
        return errors::server::read_invalid()
            .field("limit")
            .violation(format!("{requested} exceeds the maximum {PAGE_LIMIT_MAX}"))
            .fail();
    }
    let Ok(limit) = usize::try_from(requested) else {
        unreachable!("a limit at or under PAGE_LIMIT_MAX fits usize: requested={requested}")
    };
    Ok(limit)
}

const _: () = assert!(PAGE_LIMIT_MAX <= usize::MAX as u64);

/// The one checkpoint every read call passes before its own validation. `projection`
/// left the wire, so `rev` is the only field this still takes; a caller-supplied `rev`
/// needs no rule beyond what [`ReadService::at_revision`] already enforces when it
/// resolves that revision. `get_symbol`'s `scope` rule pairs with `rev` and lives there.
#[expect(
    clippy::unnecessary_wraps,
    reason = "kept as Result for symmetry with every other read validation, which every call \
              site propagates with `?`"
)]
pub(crate) fn validate_common(_rev: bool) -> Result<(), RiftError> {
    Ok(())
}

/// Why a read refuses a `scope` past `local`, or a `packages` argument, beside a revision.
pub(crate) const CURRENT_TREE_ALONE: &str = "package facts are served for the current tree alone";

/// Why a `local` read refuses a `packages` argument.
const LOCAL_SCOPE_ALONE: &str = "the local scope reads the project alone";

/// Refuses a `packages` argument no read can send: `get_symbol` and `search` share the
/// rule.
///
/// A `local` scope reads the project alone, and a revision read serves no package fact,
/// so either leaves the argument nothing to change. The list is bounded by
/// [`REQUESTED_PACKAGES_MAX`], and each entry is classified against the lengths its
/// schema advertises before it can reach the global API.
///
/// # Errors
///
/// Returns [`RiftError`] naming `packages` for the first rule the argument breaks.
pub(crate) fn validate_requested_packages(
    scope: SearchScope,
    rev: bool,
    packages: &[RequestedPackage],
) -> Result<(), RiftError> {
    match requested_packages_violation(scope, rev, packages) {
        Some(violation) => errors::server::read_invalid()
            .field("packages")
            .violation(violation)
            .fail(),
        None => Ok(()),
    }
}

/// The first rule a `packages` argument breaks, in precedence order: the read it rides
/// on, then the list bound, then each entry in list order.
fn requested_packages_violation(
    scope: SearchScope,
    rev: bool,
    packages: &[RequestedPackage],
) -> Option<String> {
    match packages {
        [] => None,
        _ if rev => Some(CURRENT_TREE_ALONE.to_owned()),
        _ if scope == SearchScope::Local => Some(LOCAL_SCOPE_ALONE.to_owned()),
        _ if packages.len() > REQUESTED_PACKAGES_MAX => Some(format!(
            "{} entries exceed the maximum {REQUESTED_PACKAGES_MAX}",
            packages.len()
        )),
        _ => packages.iter().enumerate().find_map(|(index, package)| {
            package
                .violation()
                .map(|violation| format!("entry {index} breaks {}", violation.as_ref()))
        }),
    }
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

/// The warning a read that reached `results_max` carries: whatever the bound cut,
/// a candidate before ranking or a hit after it, never reaches a page, so the
/// caller narrows the request rather than paging on.
pub(crate) fn results_truncation_warning(results_max: usize) -> ReadWarning {
    ReadWarning::ResultsTruncated {
        results_max: u64::try_from(results_max).unwrap_or(u64::MAX),
    }
}

fn wire_node(file: &IndexedFile, node: &SyntaxNode) -> Node {
    wire_node_facts(file, node.range, node.kind)
}

fn wire_node_facts(file: &IndexedFile, range: ByteRange, kind: &'static str) -> Node {
    let language = file.syntax().language();
    Node {
        id: node_id(file, range),
        symbol: symbol_for_range(file, range).map(|symbol| symbol_id(file, symbol)),
        unit: file_id(file.path()),
        language: language.clone(),
        kind: wire_kind(kind),
        facets: language_provider(language).node_facets(kind),
        range: text_range(range),
        regions: Vec::new(),
        parent: None,
        extensions: Extensions(BTreeMap::new()),
    }
}

fn symbol_node(matched: SymbolMatch<'_>) -> Node {
    matched.symbol.node_kind.map_or_else(
        || {
            let language = matched.file.syntax().language();
            Node {
                id: NodeId(node_address(matched.file, matched.symbol.range)),
                symbol: Some(symbol_id(matched.file, matched.symbol)),
                unit: file_id(matched.file.path()),
                language: language.clone(),
                kind: wire_kind(matched.symbol.kind),
                facets: vec![NodeFacet::Declaration, NodeFacet::Definition],
                range: text_range(matched.symbol.range),
                regions: Vec::new(),
                parent: None,
                extensions: Extensions(BTreeMap::new()),
            }
        },
        |kind| wire_node_facts(matched.file, matched.symbol.range, kind),
    )
}

/// Builds one hit's wire symbol, and the `symbol_disagreement` warning its retained
/// presentation disagreements raise, when it has an established identity to name in
/// one.
pub(crate) fn wire_symbol(
    index: &WorkspaceIndex,
    matched: SymbolMatch<'_>,
) -> Result<(Symbol, Option<ReadWarning>), RiftError> {
    let readable = index.assembled_symbol(matched)?;
    let symbol = assembled_wire_symbol(&readable);
    let disagreement = symbol_disagreement_warning(readable.assembled());
    Ok((symbol, disagreement))
}

fn assembled_wire_symbol(readable: &ReadableSymbol) -> Symbol {
    readable.to_protocol_symbol()
}

/// A `get_symbol` hit's own location: `path` for a project declaration or a declaration
/// whose location is unestablished, `unit` for one that belongs to a dependency, the
/// standard library, or source external to the workspace. `matched.file.path()` is the
/// project-relative path the declaration was indexed at either way; `unit` re-addresses
/// it through the source catalog for a location the hit's own `path` cannot name.
fn hit_location(
    location: Option<SourceLocationKind>,
    path: &CoreProjectPath,
) -> (Option<ProjectPath>, Option<SourceUnitId>) {
    match location {
        None | Some(SourceLocationKind::Project) => (Some(project_path(path)), None),
        Some(
            SourceLocationKind::Dependency
            | SourceLocationKind::Stdlib
            | SourceLocationKind::External,
        ) => (None, Some(source_unit_id(path))),
    }
}

#[cfg(test)]
fn wire_source_location_kind(location: &rift_core::SourceLocation) -> SourceLocationKind {
    match location {
        rift_core::SourceLocation::Project { .. } => SourceLocationKind::Project,
        rift_core::SourceLocation::Dependency { .. } => SourceLocationKind::Dependency,
        rift_core::SourceLocation::Stdlib {} => SourceLocationKind::Stdlib,
        rift_core::SourceLocation::External {} => SourceLocationKind::External,
    }
}

/// The `symbol_disagreement` warning one assembled symbol raises, when normalization
/// retained a presentation disagreement and the symbol has an established identity to
/// name in it. An unestablished symbol already says as much through its missing `id`;
/// the warning adds nothing there, so it stays silent.
fn symbol_disagreement_warning(assembled: &rift_provider::AssembledSymbol) -> Option<ReadWarning> {
    if assembled.disagreements().is_empty() {
        return None;
    }
    let identity = assembled.identity()?;
    let providers: Vec<String> = assembled
        .disagreements()
        .iter()
        .map(|disagreement| {
            disagreement
                .contribution()
                .reference()
                .provider()
                .as_str()
                .to_owned()
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let detail = format!(
        "normalization selected one presentation for this symbol; {} disagree on at \
         least one field",
        providers.join(", "),
    );
    Some(ReadWarning::SymbolDisagreement {
        symbol: SymbolId(identity.as_str().to_owned()),
        providers,
        detail,
    })
}

pub(crate) fn excerpt(file: &IndexedFile, range: ByteRange) -> String {
    let start = usize::try_from(range.start)
        .unwrap_or(file.source().len())
        .min(file.source().len());
    let end = usize::try_from(range.end)
        .unwrap_or(file.source().len())
        .min(file.source().len());
    file.source().get(start..end).unwrap_or_default().to_owned()
}

pub(crate) fn text_range(range: ByteRange) -> TextRange {
    TextRange {
        start: range.start,
        end: range.end,
    }
}

/// Composes the wire kind for one grammar fact: the provider's own kind word, as in
/// `function_item`. `language` rides beside `kind` in every payload that carries one, so
/// the wire kind carries no language prefix of its own.
fn wire_kind(kind: &str) -> ExactKind {
    ExactKind(kind.to_owned())
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
pub(crate) fn language_selects(filter: &Language, candidate: &Language) -> bool {
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
pub(crate) fn wire_index_warning(warning: &WorkspaceIndexWarning) -> ReadWarning {
    let path = warning.path();
    ReadWarning::SourceUnavailable {
        unit: Some(file_id(path)),
        detail: format!(
            "{path} {reason}, so the file is absent from the index",
            path = path.as_str(),
            reason = warning.reason(),
        ),
    }
}

/// The warnings one answer carries for the files the index left out, whole or in part:
/// the `source_unavailable` warnings for the files left out whole, the first
/// [`SOURCE_WARNINGS_MAX`] in project-path order, the order the index keeps them in, then -
/// when more were left out - one more counting the rest, which `rift server logs` names one by
/// one; and one `large_file_unparsed` naming the files held as text alone.
pub(crate) fn source_warnings(warnings: &[WorkspaceIndexWarning]) -> Vec<ReadWarning> {
    let (unparsed, left_out): (Vec<&WorkspaceIndexWarning>, Vec<&WorkspaceIndexWarning>) =
        warnings.iter().partition(|warning| warning.holds_text());
    let mut warnings: Vec<ReadWarning> = left_out
        .iter()
        .take(SOURCE_WARNINGS_MAX)
        .map(|warning| wire_index_warning(warning))
        .collect();
    let rest = left_out.len().saturating_sub(SOURCE_WARNINGS_MAX);
    if rest > 0 {
        warnings.push(ReadWarning::SourceUnavailable {
            unit: None,
            detail: format!(
                "{rest} more files are absent from the index; rift server logs names each"
            ),
        });
    }
    warnings.extend(unparsed_warning(&unparsed));
    warnings
}

/// The one warning naming the files held as text the syntax provider does not parse, at
/// most [`SOURCE_WARNINGS_MAX`] of them, or none when no file is.
fn unparsed_warning(unparsed: &[&WorkspaceIndexWarning]) -> Option<ReadWarning> {
    if unparsed.is_empty() {
        return None;
    }
    let files = unparsed
        .iter()
        .take(SOURCE_WARNINGS_MAX)
        .map(|warning| file_id(warning.path()))
        .collect();
    let detail = format!(
        "{count} files are past [providers.syntax] max_file, so the index holds their text \
         alone: search reads it, and none of their declarations were extracted; raising \
         max_file parses them",
        count = unparsed.len(),
    );
    Some(ReadWarning::LargeFileUnparsed { files, detail })
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

fn nodes_at_file(
    file: &IndexedFile,
    matched: &[SyntaxNode],
    warnings: Vec<ReadWarning>,
) -> NodesResult {
    let nodes = matched.iter().map(|node| wire_node(file, node)).collect();
    let source = matched
        .iter()
        .map(|node| excerpt(file, node.range))
        .collect();
    NodesResult {
        nodes,
        source,
        warnings,
    }
}

fn validate_node_position(file: &IndexedFile, position: u64) -> Result<(), RiftError> {
    let source_len = file.source().len() as u64;
    if position >= source_len {
        return errors::server::read_invalid()
            .field("position")
            .violation(format!(
                "{position} is at or past the file's byte length {source_len}"
            ))
            .fail();
    }
    Ok(())
}

fn node_id(file: &IndexedFile, range: ByteRange) -> NodeId {
    NodeId(node_address(file, range))
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

    /// The published index's own tree revision, truncated to its wire form. Distinct from
    /// [`Self::wire_tree_revision`], which truncates the tree revision a read captured at
    /// request time - the two agree exactly when the index is not stale.
    pub(crate) fn wire_index_tree_revision(&self) -> Digest {
        wire_digest(&self.index_tree_revision)
    }
}

/// Reads the dependency context over every path `source_policy` makes visible.
///
/// The resolvers read manifests and lockfiles, and the Python `RECORD` lists that name
/// install folders; the one program the pass runs is a standard library version probe,
/// under `[dependencies] resolution = "auto"` alone and bounded by `command_timeout`.
/// The walk is the policy's own, so a manifest the `[source]` policy or `.gitignore`
/// hides reaches no resolver.
fn resolved_context(
    root: &Path,
    source_policy: &WorkspaceSourcePolicy,
    configuration: &DependenciesConfiguration,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<DependencyContext, RiftError> {
    let visible: Vec<ProjectPath> = source_policy
        .visible_paths()?
        .iter()
        .map(project_path)
        .collect();
    let libraries = standard_libraries(source_policy, &visible);
    crate::dependency::read_workspace_context(
        root,
        &visible,
        &configuration.packages,
        crate::dependency::ResolutionPolicy::from(configuration),
        &libraries,
        cancelled,
    )
}

/// The standard libraries the workspace's languages rely on: one per library whose
/// language the `[languages]` policy selects, enabled and with its `stdlib` key on, for
/// at least one visible path. A Rust workspace holding one `.py` file therefore names
/// `stdlib/python` too, and a library two languages name goes out while either keeps
/// the key on. A path two language entries claim names neither; the index refuses it on
/// its own.
fn standard_libraries(
    source_policy: &WorkspaceSourcePolicy,
    visible: &[ProjectPath],
) -> Vec<StandardLibrary> {
    let mut libraries: Vec<StandardLibrary> = visible
        .iter()
        .filter_map(|path| path_library(source_policy, &path.0))
        .collect();
    libraries.sort_unstable();
    libraries.dedup();
    libraries
}

/// The standard library the language `[languages]` selects, enabled and with its
/// `stdlib` key on, for one path relies on; `None` for a path of no such language.
fn path_library(source_policy: &WorkspaceSourcePolicy, path: &str) -> Option<StandardLibrary> {
    source_policy
        .language_policy()
        .language_for_path(Path::new(path))
        .ok()
        .flatten()
        .filter(|language| language.enabled() && language.stdlib())
        .and_then(|language| StandardLibrary::for_language(language.identity()))
}

/// The revisions one read service captures at build time. The captured tree
/// and the indexed tree both derive from the one index the service holds, so
/// a service built here observes them equal; `CapturedRevisions::warnings`
/// guards the comparison all the same, so an index resolved apart from its
/// capture cannot lag silently.
fn captured_revisions(index: &WorkspaceIndex) -> CapturedRevisions {
    let digest = index.tree_revision();
    CapturedRevisions {
        tree_revision: digest.clone(),
        index_tree_revision: digest,
    }
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

/// First `DIGEST_WIRE_CHARS` lowercase hex characters of the SHA-256 of `source` - the sole
/// wire constructor for a witness or a `Digest`. A 64-character digest reaching the wire is a
/// defect this stays the single choke point against.
pub(crate) fn digest_hex8(source: &str) -> String {
    digest_wire_hex(&Sha256::digest(source.as_bytes()))
}

/// Truncates an already-hashed full-length hex digest to its wire form. `full` keeps
/// collision resistance for internal identity computation; only the truncated form crosses
/// the wire boundary.
///
/// # Panics
///
/// Panics when `full` is shorter than the wire form: every caller hands it a full SHA-256
/// hex rendering.
#[must_use]
pub fn wire_digest(full: &str) -> Digest {
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

/// A parsed symbol address: the language segment it files under, and its
/// decoded path and qualified name.
#[derive(Debug)]
pub(crate) struct SymbolAddress {
    pub(crate) language_segment: String,
    pub(crate) path: CoreProjectPath,
    pub(crate) qualified_name: String,
}

impl SymbolAddress {
    /// The wire symbol identity this address spells, re-encoded.
    pub(crate) fn wire_symbol(&self) -> rift_protocol::read::SymbolId {
        rift_protocol::read::SymbolId(rift_core::symbol_identity(
            &self.language_segment,
            self.path.as_str(),
            &self.qualified_name,
        ))
    }
}

/// Splits `rift://symbol/<language>/<path>/<qualified-name>` into its
/// decoded parts. The language segment is taken as spelled; resolution
/// verifies it against the addressed file's document.
pub(crate) fn parse_symbol_address(address: &str) -> Result<SymbolAddress, RiftError> {
    let remainder = address
        .strip_prefix(rift_core::constants::SYMBOL_URI_PREFIX)
        .ok_or_else(|| {
            errors::server::read_invalid()
                .field("symbol")
                .violation("not a rift symbol address")
                .error()
        })?;
    let (language_segment, remainder) = remainder.split_once('/').ok_or_else(|| {
        errors::server::read_invalid()
            .field("symbol")
            .violation("not a rift symbol address")
            .error()
    })?;
    if language_segment.is_empty() {
        return errors::server::read_invalid()
            .field("symbol")
            .violation("not a rift symbol address")
            .fail();
    }
    let (encoded_path, encoded_name) = remainder.rsplit_once('/').ok_or_else(|| {
        errors::server::read_invalid()
            .field("symbol")
            .violation("not a rift symbol address")
            .error()
    })?;
    let path = decoded(encoded_path).ok_or_else(|| {
        errors::server::read_invalid()
            .field("symbol")
            .violation("not a rift symbol address")
            .error()
    })?;
    let qualified_name = decoded(encoded_name).ok_or_else(|| {
        errors::server::read_invalid()
            .field("symbol")
            .violation("not a rift symbol address")
            .error()
    })?;
    let path = CoreProjectPath::new(path).map_err(|error| {
        errors::server::read_invalid()
            .field("symbol")
            .violation(error.detail())
            .cause(error)
            .error()
    })?;
    Ok(SymbolAddress {
        language_segment: language_segment.to_owned(),
        path,
        qualified_name,
    })
}

fn decoded(encoded: &str) -> Option<String> {
    percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::error::Error;
    use std::fs::{self, OpenOptions};
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
    use rift_index::{
        LastCapture, WorkspaceIndexPreparation, WorkspaceSourcePolicy,
        capture_digests_with_languages,
    };
    use rift_protocol::configuration::{LanguageConfiguration, WorkspaceConfiguration};
    use rift_protocol::read::{
        GetSymbolInclude, GetSymbolParams, Language, NodeFacet, NodesParams, NodesResult,
        PAGE_LIMIT_MAX, Pagination, ProjectPath, ReadWarning, RevisionId, SOURCE_WARNINGS_MAX,
        SearchScope,
    };
    use rift_syntax::{SyntaxLimits, SyntaxSource, registry};
    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        Arc, DependenciesConfiguration, DependencyResolution, HistoryConfiguration,
        REQUESTED_PACKAGES_MAX, ReadService, RequestedPackage, RiftError, WorkspaceIndex,
        WorkspaceIndexLimits, accepted_limit, errors, excerpt, file_id,
        validate_requested_packages, wire_node,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    #[test]
    fn symbol_address_refuses_empty_language_and_decoded_invalid_paths() {
        for (address, expected_violation, expected_cause) in [
            (
                "rift://symbol//lib.rs/beacon",
                Some("not a rift symbol address"),
                None,
            ),
            (
                "rift://symbol/rust/%2Flib.rs/beacon",
                None,
                Some(rift_error::errors::core::path_absolute::SLUG),
            ),
            (
                "rift://symbol/rust/src%2F..%2Flib.rs/beacon",
                None,
                Some(rift_error::errors::core::path_dot_segment::SLUG),
            ),
        ] {
            let error = super::parse_symbol_address(address)
                .map(|_| ())
                .expect_err("invalid address");
            assert_eq!(error.slug(), errors::server::read_invalid::SLUG);
            assert!(
                error
                    .context()
                    .any(|(key, value)| key == "field" && value == "symbol")
            );
            if let Some(expected) = expected_violation {
                assert!(
                    error
                        .context()
                        .any(|(key, value)| key == "violation" && value == expected)
                );
            }
            if let Some(expected) = expected_cause {
                let cause = std::error::Error::source(&error)
                    .and_then(|source| source.downcast_ref::<RiftError>())
                    .expect("invalid path error remains its cause");
                assert_eq!(cause.slug(), expected);
            }
        }
    }

    /// A read snapshot over `root` under `limits` and `languages`, built through the
    /// same entry point the server itself uses.
    fn reads_with(
        root: &std::path::Path,
        limits: WorkspaceIndexLimits,
        text_inclusion: &rift_core::TextFileInclusion,
        languages: &LanguageFileSelections,
    ) -> Result<ReadService, super::RiftError> {
        ReadService::build_with_languages(
            root,
            limits,
            &SourceVisibility::default(),
            text_inclusion,
            languages,
            HistoryConfiguration::default(),
            DependenciesConfiguration {
                resolution: DependencyResolution::Static,
                ..DependenciesConfiguration::default()
            },
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
    fn the_context_is_reread_only_when_one_of_its_inputs_changes() -> TestResult {
        let directory = TempDir::new()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;

        let changed = |path: &str| {
            rift_index::PathChanges::between(
                &rift_index::WorkspaceDigests::new([]),
                &rift_index::WorkspaceDigests::new([(
                    rift_core::ProjectPath::new(path).expect("fixture path"),
                    rift_index::FileDigest::of(b"changed"),
                )]),
            )
        };

        let policy = service.filesystem_policy("read the dependency context")?;
        let kept =
            service.context_after(policy, &changed("src/lib.rs"), &service.index, &|| false)?;
        let reread =
            service.context_after(policy, &changed("Cargo.toml"), &service.index, &|| false)?;

        assert!(
            std::sync::Arc::ptr_eq(&kept, &service.context),
            "a change outside the context's inputs keeps the standing context"
        );
        assert!(
            !std::sync::Arc::ptr_eq(&reread, &service.context),
            "a manifest change reads the context again"
        );
        Ok(())
    }

    #[test]
    fn the_context_is_reread_when_a_language_gains_its_first_path_or_loses_its_last() -> TestResult
    {
        let directory = TempDir::new()?;
        let manifest = "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        fs::write(directory.path().join("Cargo.toml"), manifest)?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("src/extra.rs"), "pub fn extra() {}\n")?;
        let (limits, visibility) = (WorkspaceIndexLimits::default(), SourceVisibility::default());
        let text = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service = ReadService::build(directory.path(), limits, &visibility, &text, history)?;
        let digests = |paths: &[(&str, &[u8])]| {
            rift_index::WorkspaceDigests::new(paths.iter().map(|(path, bytes)| {
                (
                    rift_core::ProjectPath::new(*path).expect("fixture path"),
                    rift_index::FileDigest::of(bytes),
                )
            }))
        };
        let python = |service: &ReadService| {
            service
                .context
                .standard_libraries()
                .contains(&rift_dependency::StandardLibrary::Python)
        };
        assert!(
            !python(&service),
            "a Rust workspace names no Python library"
        );

        let tool: &[u8] = b"print('probe')\n";
        fs::write(directory.path().join("tool.py"), tool)?;
        let gained = service.rebuilt(&rift_index::PathChanges::between(
            &digests(&[]),
            &digests(&[("tool.py", tool)]),
        ))?;
        assert!(
            !std::sync::Arc::ptr_eq(&gained.context, &service.context) && python(&gained),
            "the first Python path reads the context again and names `stdlib/python`"
        );

        fs::remove_file(directory.path().join("src/extra.rs"))?;
        let kept = gained.rebuilt(&rift_index::PathChanges::between(
            &digests(&[("src/extra.rs", b"pub fn extra() {}\n".as_slice())]),
            &digests(&[]),
        ))?;
        assert!(
            std::sync::Arc::ptr_eq(&kept.context, &gained.context),
            "removing one Rust path of two keeps the standing context"
        );

        fs::remove_file(directory.path().join("tool.py"))?;
        let lost = kept.rebuilt(&rift_index::PathChanges::between(
            &digests(&[("tool.py", tool)]),
            &digests(&[]),
        ))?;
        assert!(
            !std::sync::Arc::ptr_eq(&lost.context, &kept.context) && !python(&lost),
            "removing the last Python path reads the context again without it"
        );
        Ok(())
    }

    /// `[languages.<name>] stdlib = false` leaves that language's library out of the
    /// context, while a library another present language names with the key on stays: a
    /// `.ts` and a `.js` file with the key off on `typescript` still name `stdlib/node`.
    #[test]
    fn the_stdlib_key_leaves_a_library_out_unless_another_present_language_names_it() -> TestResult
    {
        use rift_dependency::StandardLibrary::{Node, Python};

        let directory = TempDir::new()?;
        fs::write(directory.path().join("tool.py"), "print('probe')\n")?;
        fs::write(directory.path().join("app.ts"), "export const a = 1;\n")?;
        fs::write(directory.path().join("app.js"), "export const b = 2;\n")?;
        let read = |switches: &[(&str, bool)]| -> TestResult<ReadService> {
            let mut configuration = WorkspaceConfiguration::default();
            for (name, stdlib) in switches {
                configuration.languages.insert(
                    (*name).to_owned(),
                    LanguageConfiguration {
                        stdlib: *stdlib,
                        ..LanguageConfiguration::default()
                    },
                );
            }
            let languages = LanguageFileSelections::from(&configuration);
            let text_inclusion = rift_core::TextFileInclusion::default();
            let limits = WorkspaceIndexLimits::default();
            let reads = reads_with(directory.path(), limits, &text_inclusion, &languages)?;
            Ok(reads)
        };
        let named = |service: &ReadService| -> Vec<String> {
            service
                .context
                .entries()
                .iter()
                .map(|entry| format!("{}/{}", entry.manager, entry.name))
                .collect()
        };

        let defaults = read(&[])?;
        let python_off = read(&[("python", false)])?;
        let typescript_off = read(&[("typescript", false)])?;
        let both_off = read(&[("typescript", false), ("javascript", false)])?;

        assert_eq!(
            named(&defaults),
            ["npm/typescript", "stdlib/node", "stdlib/python"]
        );
        assert_eq!(
            python_off
                .context
                .standard_libraries()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [Node]
        );
        assert_eq!(
            typescript_off
                .context
                .standard_libraries()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [Node, Python],
            "`javascript` names the Node.js library with its key on"
        );
        assert_eq!(named(&both_off), ["stdlib/python"]);
        Ok(())
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

        assert_eq!(error.slug(), errors::server::read_unsupported::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "capability" && value == "rust files" })
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

        assert_eq!(error.slug(), errors::index::workspace_too_many_files::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "maximum" && value == "1")
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

    /// The content digests are the ones the build took from each file's bytes, so a
    /// comparison against the lexical store's recorded digests reads no file again.
    #[test]
    fn content_digests_answer_the_digest_of_each_indexed_files_bytes() -> TestResult {
        let (directory, service) = fixture()?;
        let path = rift_core::ProjectPath::new("src/lib.rs")?;
        let bytes = fs::read(directory.path().join("src/lib.rs"))?;
        assert_eq!(
            service.content_digests().get(&path),
            Some(rift_index::FileDigest::of(&bytes))
        );
        Ok(())
    }

    /// `ReadService::relationships` is a pass-through onto the underlying index's own
    /// store: an independently built [`WorkspaceIndex`] over the identical source must
    /// report the same adjacency the service serves.
    #[test]
    fn relationships_pass_through_serves_the_same_edges_the_index_holds() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn alpha() {}\npub fn beta() { alpha(); }\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let independent_index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )?;

        let beta = rift_core::SymbolId::new("rift://symbol/rust/src/lib.rs/beta")?;
        let alpha = rift_core::SymbolId::new("rift://symbol/rust/src/lib.rs/alpha")?;
        assert_eq!(
            service.relationships().is_empty(),
            independent_index.relationships().is_empty(),
            "no provider publishes a resolved reference, so both stores are empty"
        );
        assert_eq!(
            service.relationships().outgoing(&beta),
            independent_index.relationships().outgoing(&beta)
        );
        assert_eq!(
            service.relationships().incoming(&alpha),
            independent_index.relationships().incoming(&alpha)
        );
        Ok(())
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
        assert_eq!(value["nodes"][0]["language"], "rust");
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
    fn nodes_reparse_captured_source_with_selected_provider_and_bounds() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source_directory = directory.path().join("src");
        fs::create_dir(&source_directory)?;
        let path = rift_core::ProjectPath::new("src/lib.rs")?;
        let source = "pub struct Beacon;\nimpl Beacon { pub fn signal(&self) {} }\n";
        let source_path = directory.path().join(path.as_str());
        fs::write(&source_path, source)?;
        let syntax_limits = SyntaxLimits::new(128, 64, 16)?;
        let limits = WorkspaceIndexLimits::default().with_syntax(syntax_limits);
        let service = ReadService::build(
            directory.path(),
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let file = service
            .index()
            .file(&path)
            .ok_or("indexed source missing")?;
        let provider = registry::provider_for_language(file.syntax().language())
            .ok_or("selected syntax provider missing")?;
        let complete = provider.analyze(
            SyntaxSource {
                path: &path,
                text: file.source(),
            },
            syntax_limits,
        )?;
        let position = 12;
        let matched = complete.nodes_at(position);
        let expected_nodes = matched
            .iter()
            .map(|node| wire_node(file, node))
            .collect::<Vec<_>>();
        let expected_source = matched
            .iter()
            .map(|node| excerpt(file, node.range))
            .collect::<Vec<_>>();

        fs::write(&source_path, "pub fn replacement() {}\n".repeat(40))?;
        let actual = service.nodes(NodesParams {
            path: ProjectPath(path.as_str().to_owned()),
            position,
            rev: None,
        })?;

        assert_eq!(actual.nodes, expected_nodes);
        assert_eq!(actual.source, expected_source);
        assert!(actual.source.iter().any(|part| part.contains("Beacon")));
        Ok(())
    }

    #[test]
    fn nodes_reparse_with_path_selected_typescript_dialect() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source_directory = directory.path().join("src");
        fs::create_dir(&source_directory)?;
        let path = rift_core::ProjectPath::new("src/View.tsx")?;
        let source = "const View = () => <section />;\n";
        fs::write(directory.path().join(path.as_str()), source)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let file = service
            .index()
            .file(&path)
            .ok_or("indexed TSX source missing")?;
        assert_eq!(file.syntax().language().dialect.as_deref(), Some("tsx"));
        let provider = registry::provider_for_language(file.syntax().language())
            .ok_or("selected TSX provider missing")?;
        let complete = provider.analyze(
            SyntaxSource {
                path: &path,
                text: file.source(),
            },
            service.index().limits().syntax(),
        )?;
        let position = u64::try_from(source.find("section").ok_or("fixture tag missing")?)?;
        let matched = complete.nodes_at(position);
        let expected_nodes = matched
            .iter()
            .map(|node| wire_node(file, node))
            .collect::<Vec<_>>();
        let actual = service.nodes(NodesParams {
            path: ProjectPath(path.as_str().to_owned()),
            position,
            rev: None,
        })?;

        assert_eq!(actual.nodes, expected_nodes);
        assert!(
            actual
                .nodes
                .iter()
                .any(|node| node.language.dialect.as_deref() == Some("tsx"))
        );
        Ok(())
    }

    #[test]
    fn nodes_parse_a_discovered_source_before_its_batch_is_prepared() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() { let value = 1; }\n",
        )?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text = TextFileInclusion::default();
        let languages = LanguageFileSelections::default();
        let mut preparation =
            WorkspaceIndexPreparation::new(directory.path(), limits, &text, &languages)?;
        preparation.discover(&visibility, &|| false)?;
        let source_policy = WorkspaceSourcePolicy::build_with_languages_cancellable(
            directory.path(),
            limits,
            &visibility,
            &text,
            &languages,
            &|| false,
        )?;
        let partial = ReadService::from_prepared_index(
            preparation.empty_snapshot()?,
            Some(Arc::new(source_policy)),
            Arc::new(rift_dependency::DependencyContext::default()),
            HistoryConfiguration::default(),
            DependenciesConfiguration {
                resolution: DependencyResolution::Static,
                ..DependenciesConfiguration::default()
            },
        );
        let settled = reads_with(directory.path(), limits, &text, &languages)?;
        let params: NodesParams = serde_json::from_value(json!({
            "path": "lib.rs",
            "position": 8
        }))?;

        let actual = partial.nodes_for_preparation(params.clone())?;
        let expected = settled.nodes(params)?;
        assert_eq!(actual.nodes, expected.nodes);
        assert_eq!(actual.source, expected.source);
        assert!(!actual.nodes.is_empty());
        Ok(())
    }

    fn partial_nodes_service(root: &Path, limits: WorkspaceIndexLimits) -> TestResult<ReadService> {
        let visibility = SourceVisibility::default();
        let text = TextFileInclusion::default();
        let languages = LanguageFileSelections::default();
        let mut preparation = WorkspaceIndexPreparation::new(root, limits, &text, &languages)?;
        preparation.discover(&visibility, &|| false)?;
        let source_policy = WorkspaceSourcePolicy::build_with_languages_cancellable(
            root,
            limits,
            &visibility,
            &text,
            &languages,
            &|| false,
        )?;
        Ok(ReadService::from_prepared_index(
            preparation.empty_snapshot()?,
            Some(Arc::new(source_policy)),
            Arc::new(rift_dependency::DependencyContext::default()),
            HistoryConfiguration::default(),
            DependenciesConfiguration {
                resolution: DependencyResolution::Static,
                ..DependenciesConfiguration::default()
            },
        ))
    }

    #[test]
    fn nodes_results_match_with_unrelated_invalid_and_unparsed_sources() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/kept.rs"), "pub fn kept() {}\n")?;
        fs::write(directory.path().join("src/invalid.rs"), [0xff])?;
        fs::write(
            directory.path().join("src/large.rs"),
            format!("// {}\npub fn large() {{}}\n", "x".repeat(256)),
        )?;
        let syntax = SyntaxLimits::new(64, 4_096, 64)?;
        let limits = WorkspaceIndexLimits::default().with_syntax(syntax);
        let partial = partial_nodes_service(directory.path(), limits)?;
        let settled = reads_with(
            directory.path(),
            limits,
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        )?;
        let params = NodesParams {
            path: ProjectPath("src/kept.rs".to_owned()),
            position: 0,
            rev: None,
        };

        let actual = partial.nodes_for_preparation(params.clone())?;
        let expected = settled.nodes(params)?;
        assert_eq!(actual, expected);
        assert!(actual.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn nodes_preparation_returns_content_unavailable_for_own_skipped_source() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("invalid.rs"), [0xff])?;
        fs::write(
            directory.path().join("large.rs"),
            format!("// {}\npub fn large() {{}}\n", "x".repeat(256)),
        )?;
        let syntax = SyntaxLimits::new(64, 4_096, 64)?;
        let limits = WorkspaceIndexLimits::default().with_syntax(syntax);
        let partial = partial_nodes_service(directory.path(), limits)?;
        let settled = reads_with(
            directory.path(),
            limits,
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        )?;

        for path in ["invalid.rs", "large.rs"] {
            let params = NodesParams {
                path: ProjectPath(path.to_owned()),
                position: 0,
                rev: None,
            };
            let partial_error = partial
                .nodes_for_preparation(params.clone())
                .expect_err("an omitted source cannot answer nodes");
            let settled_error = settled
                .nodes(params)
                .expect_err("settled index omits the same source");
            assert_eq!(
                partial_error.slug(),
                errors::server::read_source_unavailable::SLUG
            );
            assert_eq!(
                settled_error.slug(),
                errors::server::read_source_unavailable::SLUG
            );
        }

        let large_directory = tempfile::tempdir()?;
        fs::write(
            large_directory.path().join("large.rs"),
            "pub fn large() {}\n",
        )?;
        let limits = WorkspaceIndexLimits::new(64, 8, 8_388_608, 16, 64)?;
        let partial = partial_nodes_service(large_directory.path(), limits)?;
        let settled = reads_with(
            large_directory.path(),
            limits,
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        )?;
        let params = NodesParams {
            path: ProjectPath("large.rs".to_owned()),
            position: 0,
            rev: None,
        };
        let partial_error = partial
            .nodes_for_preparation(params.clone())
            .expect_err("a file past the catalog byte bound cannot answer nodes");
        let settled_error = settled
            .nodes(params)
            .expect_err("settled index leaves the same file out");
        assert_eq!(
            partial_error.slug(),
            errors::server::read_source_unavailable::SLUG
        );
        assert_eq!(
            settled_error.slug(),
            errors::server::read_source_unavailable::SLUG
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn nodes_preparation_preserves_unrelated_source_read_errors() -> TestResult {
        let directory = tempfile::tempdir()?;
        let partial = partial_nodes_service(directory.path(), WorkspaceIndexLimits::default())?;
        fs::create_dir(directory.path().join("blocked.rs"))?;
        let error = partial
            .nodes_for_preparation(NodesParams {
                path: ProjectPath("blocked.rs".to_owned()),
                position: 0,
                rev: None,
            })
            .expect_err("reading a directory as source fails");
        assert_eq!(error.slug(), errors::index::workspace_filesystem::SLUG);
        Ok(())
    }

    #[test]
    fn nodes_keep_stale_index_warning_without_unrelated_source_warnings() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("kept.rs"), "pub fn kept() {}\n")?;
        fs::write(directory.path().join("invalid.rs"), [0xff])?;
        let mut service = nodes_service(directory.path(), &SourceVisibility::default())?;
        service.revisions = super::CapturedRevisions {
            tree_revision: "bb".repeat(32),
            index_tree_revision: "aa".repeat(32),
        };

        let result = nodes_at_root(&service, "kept.rs")?;
        assert_eq!(result.warnings, service.revisions.warnings());
        assert!(matches!(
            result.warnings.as_slice(),
            [ReadWarning::StaleIndex { .. }]
        ));
        Ok(())
    }

    #[test]
    fn nodes_for_preparation_refuses_a_known_declaration_bound_omission() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("a.rs"), "pub fn first() {}\n")?;
        fs::write(directory.path().join("b.rs"), "pub fn later() {}\n")?;
        let limits = WorkspaceIndexLimits::new(64, 1_048_576, 8_388_608, 16, 64)?
            .with_workspace_bounds(64, 8_388_608, 1)?;
        let settled = reads_with(
            directory.path(),
            limits,
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        )?;
        let params = NodesParams {
            path: ProjectPath("b.rs".to_owned()),
            position: 0,
            rev: None,
        };
        assert!(
            settled
                .file_record(&rift_core::ProjectPath::new("b.rs")?)
                .is_some()
        );
        let error = settled
            .nodes_for_preparation(params)
            .expect_err("a recorded declaration-bound omission must not be reparsed");
        assert_eq!(error.slug(), errors::server::read_source_unavailable::SLUG);
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
            assert_eq!(error.slug(), errors::server::read_invalid::SLUG);
            assert!(
                error
                    .context()
                    .any(|(key, value)| key == "field" && value == "position")
            );
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
        let (directory, service) = fixture()?;
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
        let text = std::fs::read_to_string(directory.path().join("src/lib.rs"))?;
        for (node, source) in result.nodes.iter().zip(result.source.iter()) {
            let start = usize::try_from(node.range.start)?;
            let end = usize::try_from(node.range.end)?;
            assert_eq!(
                source.as_str(),
                &text[start..end],
                "each excerpt must span its own node's range, not the requested position"
            );
        }
        Ok(())
    }

    #[test]
    fn hit_location_answers_exactly_one_address_for_every_location_kind() -> TestResult {
        let path = rift_core::ProjectPath::new("src/lib.rs")?;
        for kind in [
            None,
            Some(rift_protocol::read::SourceLocationKind::Project),
            Some(rift_protocol::read::SourceLocationKind::Dependency),
            Some(rift_protocol::read::SourceLocationKind::Stdlib),
            Some(rift_protocol::read::SourceLocationKind::External),
        ] {
            let (project, unit) = super::hit_location(kind, &path);
            assert!(
                project.is_some() != unit.is_some(),
                "exactly one of path and unit per hit: kind={kind:?}, path={project:?}, unit={unit:?}"
            );
        }
        let (project, unit) = super::hit_location(
            Some(rift_protocol::read::SourceLocationKind::Project),
            &path,
        );
        assert_eq!(project.map(|p| p.0), Some("src/lib.rs".to_owned()));
        assert_eq!(unit, None);
        let (project, unit) = super::hit_location(
            Some(rift_protocol::read::SourceLocationKind::Dependency),
            &path,
        );
        assert_eq!(project, None);
        assert!(
            unit.is_some(),
            "a dependency hit re-addresses through the source catalog"
        );
        Ok(())
    }

    #[test]
    fn get_symbol_span_is_set_whether_or_not_the_body_was_requested() -> TestResult {
        let (_directory, service) = fixture()?;
        let with_body: GetSymbolParams =
            serde_json::from_value(json!({ "name": "signal", "include": ["source"] }))?;
        let without_body: GetSymbolParams =
            serde_json::from_value(json!({ "name": "signal", "include": [] }))?;
        let with_body_value = serde_json::to_value(service.get_symbol(&with_body)?)?;
        let without_body_value = serde_json::to_value(service.get_symbol(&without_body)?)?;
        assert!(
            without_body_value["hits"][0].get("source").is_none(),
            "include: [] must carry no source excerpt"
        );
        assert_eq!(
            with_body_value["hits"][0]["path"], without_body_value["hits"][0]["path"],
            "the path must not depend on include"
        );
        assert_eq!(
            with_body_value["hits"][0]["range"], without_body_value["hits"][0]["range"],
            "the range must not depend on include"
        );
        assert_eq!(without_body_value["hits"][0]["path"], json!("src/lib.rs"));
        assert!(
            without_body_value["hits"][0]["range"]["start"]
                .as_u64()
                .is_some_and(|start| start
                    < without_body_value["hits"][0]["range"]["end"]
                        .as_u64()
                        .unwrap_or_default()),
            "the range must name a real byte range: {without_body_value:#}"
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
        let full = service.index().tree_revision();
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
    fn symbol_read_returns_normalized_identity_and_omits_the_common_origin() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let symbol = &value["hits"][0]["symbol"];

        assert_eq!(symbol["name"], "Beacon");
        assert_eq!(symbol["visibility"], "pub");
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
        assert!(
            symbol.get("origin").is_none(),
            "a project-authored symbol with no package must omit origin entirely: {symbol}"
        );
        assert_eq!(value["hits"][0]["source"], "pub struct Beacon;");
        assert!(
            value.get("warnings").is_none(),
            "a single-provider fixture with no disagreement raises no warning: {value}"
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
            serde_json::from_value(json!({"name": "signal", "include": ["source"]}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;

        let symbol = &value["hits"][0]["symbol"];
        assert_eq!(symbol["language"], json!("rust"));
        assert_eq!(symbol["kind"], json!("function"));
        assert_eq!(symbol["facets"], json!(["value", "callable", "public"]));
        assert_eq!(symbol["visibility"], json!("pub"));
        assert_eq!(
            symbol["container"],
            json!("rift://symbol/rust/src/lib.rs/Beacon")
        );

        let node = value["hits"][0]["node"]
            .as_str()
            .ok_or("node must be the bare witnessed address string")?;
        assert!(
            node.starts_with("rift://node/rust/src/lib.rs@"),
            "the address embeds the language, path, and range: {node}"
        );

        let top_level: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let top_value = serde_json::to_value(service.get_symbol(&top_level)?)?;
        let beacon = &top_value["hits"][0]["symbol"];
        assert_eq!(beacon["kind"], json!("struct"));
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
        params.include = vec![GetSymbolInclude::History];
        let error = service
            .get_symbol(&params)
            .expect_err("a workspace without a repository cannot serve history");
        assert_eq!(error.slug(), errors::history::unversioned::SLUG);
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
                ..HistoryConfiguration::default()
            },
        )?;
        let mut params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;
        params.include = vec![GetSymbolInclude::History];
        let error = service
            .get_symbol(&params)
            .expect_err("a disabled history provider must refuse");
        assert_eq!(error.slug(), errors::server::read_unsupported::SLUG);
        assert!(error.context().any(|(key, value)| {
            key == "capability" && value == "symbol history (providers.history disabled)"
        }));
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
            serde_json::from_value(json!({"name": "beacon", "include": ["history"]}))?;
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
            serde_json::from_value(json!({"name": "beacon", "include": ["history"]}))?;
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

    /// `include: ["source", "history"]` carries both: the hit's node identity, source
    /// excerpt, and timeline all ride the same answer.
    #[test]
    fn get_symbol_include_both_carries_source_and_history_together() -> TestResult {
        let directory = timeline_fixture()?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "beacon", "include": ["source", "history"]}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let hit = &value["hits"][0];
        assert!(
            !hit["node"].is_null(),
            "include: [\"source\"] must carry node: {hit:#}"
        );
        assert!(
            !hit["source"].is_null(),
            "include: [\"source\"] must carry source: {hit:#}"
        );
        assert!(
            !hit["history"].is_null(),
            "include: [\"history\"] must carry history: {hit:#}"
        );
        Ok(())
    }

    /// An omitted `include` serves exactly what `["source"]` serves; `[]` serves neither.
    #[test]
    fn get_symbol_include_omitted_matches_source_and_empty_serves_neither() -> TestResult {
        let (_directory, service) = fixture()?;
        let omitted: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;
        let explicit: GetSymbolParams =
            serde_json::from_value(json!({"name": "beacon", "include": ["source"]}))?;
        let empty: GetSymbolParams =
            serde_json::from_value(json!({"name": "beacon", "include": []}))?;
        let omitted = serde_json::to_value(service.get_symbol(&omitted)?)?;
        let explicit = serde_json::to_value(service.get_symbol(&explicit)?)?;
        let empty = serde_json::to_value(service.get_symbol(&empty)?)?;
        assert_eq!(
            omitted, explicit,
            "an omitted include answers as include: [\"source\"]"
        );
        let hit = &empty["hits"][0];
        assert!(
            hit.get("source").is_none() && hit.get("node").is_none(),
            "an explicit empty include serves neither source nor node: {hit:#}"
        );
        assert!(
            !omitted["hits"][0]["source"].is_null(),
            "the omitted form serves source: {omitted:#}"
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
        assert_eq!(
            missing.expect_err("missing source must fail").slug(),
            rift_error::errors::server::read_not_found::SLUG
        );
        Ok(())
    }

    /// One read service over `directory` under `visibility`, the shape every
    /// `nodes` classification test builds.
    fn nodes_service(
        directory: &std::path::Path,
        visibility: &SourceVisibility,
    ) -> Result<ReadService, super::RiftError> {
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

    fn nodes_at_root(service: &ReadService, path: &str) -> Result<NodesResult, super::RiftError> {
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
        assert_eq!(error.slug(), errors::server::read_unclaimed_extension::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "extension" && value == "lock files")
        );
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
            (error.slug() == rift_error::errors::server::read_not_found::SLUG),
            "the workspace asked for this path to be invisible, so nodes cannot name a \
             capability for it: {error}"
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
            (error.slug() == rift_error::errors::server::read_not_found::SLUG),
            "no file stands at that path, so there is no capability to name: {error}"
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
        assert_eq!(error.slug(), errors::server::read_unclaimed_extension::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "extension" && value == "files with no extension" })
        );
        Ok(())
    }

    /// A workspace holding one UTF-8-invalid source file beside a valid one: addressing
    /// the invalid file directly answers `content_unavailable`, a genuinely absent sibling
    /// path still answers `not_found`, and a valid file's node result names neither.
    #[test]
    fn nodes_distinguishes_invalid_utf8_from_absent_and_still_serves_and_warns() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn kept() {}\n")?;
        fs::write(directory.path().join("src/invalid.rs"), [0xff])?;
        let service = nodes_service(directory.path(), &SourceVisibility::default())?;

        let invalid = nodes_at_root(&service, "src/invalid.rs")
            .expect_err("the addressed file exists but cannot be read");
        assert!(
            invalid.slug() == rift_error::errors::server::read_source_unavailable::SLUG,
            "an invalid-UTF-8 path answers content_unavailable, not not_found: {invalid}"
        );
        assert!(
            invalid
                .context()
                .any(|(key, value)| { key == "path" && value == "src/invalid.rs" })
        );

        let absent = nodes_at_root(&service, "src/missing.rs")
            .expect_err("a path nothing claims must still fail");
        assert!(
            absent.slug() == rift_error::errors::server::read_not_found::SLUG,
            "an absent sibling path is unaffected: {absent}"
        );
        assert!(
            absent
                .context()
                .any(|(key, value)| key == "path" && value == "src/missing.rs")
        );

        let kept = nodes_at_root(&service, "src/lib.rs")?;
        assert!(kept.warnings.is_empty());
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
                unit: Some(file_id(&invalid_path)),
                detail: "src/invalid.rs holds bytes that are not valid UTF-8, so the file is \
                         absent from the index"
                    .to_owned(),
            }),
            "get_symbol's answer names the skipped file too: {:?}",
            result.warnings
        );
        Ok(())
    }

    /// Nine files the index leaves out produce eight `source_unavailable` warnings in
    /// project-path order and one more counting the rest; eight produce eight alone.
    #[test]
    fn source_warnings_are_bounded_per_answer_with_one_counting_the_rest() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub struct Beacon;\n")?;
        for index in 0..=SOURCE_WARNINGS_MAX {
            fs::write(directory.path().join(format!("invalid-{index}.rs")), [0xff])?;
        }
        let service = nodes_service(directory.path(), &SourceVisibility::default())?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let result = service.get_symbol(&params)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.warnings.len(), SOURCE_WARNINGS_MAX + 1);
        let named: Vec<&str> = result
            .warnings
            .iter()
            .filter_map(|warning| match warning {
                ReadWarning::SourceUnavailable {
                    unit: Some(unit), ..
                } => Some(unit.0.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(named.len(), SOURCE_WARNINGS_MAX);
        assert!(
            named.windows(2).all(|pair| pair[0] < pair[1]),
            "the named files come in project-path order: {named:?}"
        );
        assert_eq!(
            result.warnings.last(),
            Some(&ReadWarning::SourceUnavailable {
                unit: None,
                detail: "1 more files are absent from the index; rift server logs names each"
                    .to_owned(),
            })
        );

        let past_the_bound = format!("invalid-{SOURCE_WARNINGS_MAX}.rs");
        fs::remove_file(directory.path().join(past_the_bound))?;
        let service = nodes_service(directory.path(), &SourceVisibility::default())?;
        let result = service.get_symbol(&params)?;
        assert_eq!(result.warnings.len(), SOURCE_WARNINGS_MAX);
        assert!(
            result.warnings.iter().all(|warning| matches!(
                warning,
                ReadWarning::SourceUnavailable { unit: Some(_), .. }
            )),
            "exactly the bound leaves no count warning: {:?}",
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

        assert_eq!(
            error.slug(),
            rift_error::errors::index::workspace_invalid_root::SLUG
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn nodes_rejects_path_outside_project_root() -> TestResult {
        let (_directory, service) = fixture()?;
        let error = service
            .nodes(NodesParams {
                path: ProjectPath("/etc/passwd".to_owned()),
                position: 0,
                rev: None,
            })
            .expect_err("absolute path must fail");
        assert_eq!(error.slug(), rift_error::errors::server::read_invalid::SLUG);
        assert_eq!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<RiftError>())
                .expect("path validation error remains its cause")
                .slug(),
            rift_error::errors::core::path_absolute::SLUG
        );
        Ok(())
    }

    /// `scope` is served again: every member parses under its `snake_case` spelling.
    #[test]
    fn get_symbol_accepts_scope_values() -> TestResult {
        for (spelling, scope) in [
            ("local", SearchScope::Local),
            ("global", SearchScope::Global),
            ("all", SearchScope::All),
        ] {
            let params: GetSymbolParams =
                serde_json::from_value(json!({"name": "Beacon", "scope": spelling}))?;
            assert_eq!(params.scope, scope, "scope {spelling}");
        }
        Ok(())
    }

    #[test]
    fn get_symbol_scope_defaults_to_local() -> TestResult {
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        assert_eq!(params.scope, SearchScope::Local);
        Ok(())
    }

    /// One project whose `src/lib.rs` holds `source` alone.
    pub(crate) fn project_fixture(source: &str) -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), source)?;
        let visibility = SourceVisibility::default();
        let text_inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let limits = WorkspaceIndexLimits::default();
        let root = directory.path();
        let service = ReadService::build(root, limits, &visibility, &text_inclusion, history)?;
        Ok((directory, service))
    }

    /// One project holding `pub fn beacon` alone.
    fn beacon_fixture() -> TestResult<(TempDir, ReadService)> {
        project_fixture("pub fn beacon() {}\n")
    }

    fn scoped(name: &str, scope: &str) -> TestResult<GetSymbolParams> {
        let request = json!({"name": name, "scope": scope, "limit": 10});
        Ok(serde_json::from_value(request)?)
    }

    /// An omitted `scope` answers the project index alone, by path, and no dependency
    /// warning rides.
    #[test]
    fn get_symbol_default_scope_answers_the_project_alone() -> TestResult {
        let (_directory, service) = beacon_fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;
        let result = service.get_symbol(&params)?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(
            result.hits[0].path,
            Some(ProjectPath("src/lib.rs".to_owned()))
        );
        assert_eq!(result.hits[0].unit, None);
        assert!(
            result.warnings.is_empty(),
            "the project scope carries no dependency warning: {:?}",
            result.warnings
        );
        Ok(())
    }

    /// Package facts come from the global index, so the snapshot answers a `global`
    /// lookup with no hit and an `all` lookup with the project's own, and neither
    /// carries a package warning of its own: the global route adds those.
    #[test]
    fn get_symbol_global_scope_answers_no_project_hit_and_all_answers_the_project() -> TestResult {
        let (_directory, service) = beacon_fixture()?;

        let global = service.get_symbol(&scoped("beacon", "global")?)?;
        let all = service.get_symbol(&scoped("beacon", "all")?)?;

        assert!(global.hits.is_empty(), "{global:?}");
        assert_eq!(
            global.pagination,
            Pagination {
                page_index: 0,
                total_pages: 0
            }
        );
        assert!(global.warnings.is_empty(), "{:?}", global.warnings);
        assert_eq!(all.hits.len(), 1);
        assert_eq!(all.hits[0].path, Some(ProjectPath("src/lib.rs".to_owned())));
        assert!(all.warnings.is_empty(), "{:?}", all.warnings);
        Ok(())
    }

    #[test]
    fn get_symbol_rev_with_a_global_scope_refuses_naming_scope() -> TestResult {
        let (_directory, service) = beacon_fixture()?;
        for scope in ["global", "all"] {
            let params: GetSymbolParams =
                serde_json::from_value(json!({"name": "beacon", "scope": scope, "rev": "main"}))?;

            let error = service
                .get_symbol(&params)
                .expect_err("rev pairs with the project scope alone");

            assert!(
                (error.slug() == rift_error::errors::server::read_invalid::SLUG),
                "scope {scope}: {error}"
            );
            assert_eq!(error.slug(), errors::server::read_invalid::SLUG);
        }
        Ok(())
    }

    /// One `get_symbol` request, `arguments` laid over `name: "beacon"`.
    fn lookup(arguments: &serde_json::Value) -> TestResult<GetSymbolParams> {
        let mut request = json!({"name": "beacon"});
        for (key, value) in arguments.as_object().ok_or("arguments are an object")? {
            request[key] = value.clone();
        }
        Ok(serde_json::from_value(request)?)
    }

    fn packages_violation(error: &RiftError) -> Option<String> {
        if error.slug() != errors::server::read_invalid::SLUG
            || !error
                .context()
                .any(|(key, value)| key == "field" && value == "packages")
        {
            return None;
        }
        error
            .context()
            .find_map(|(key, value)| (key == "violation").then_some(value))
    }

    /// A `local` read consults no package, so `packages` beside it, spelled or omitted,
    /// refuses naming the field.
    #[test]
    fn get_symbol_packages_beside_the_local_scope_refuses_naming_packages() -> TestResult {
        let (_directory, service) = beacon_fixture()?;
        let packages = json!([{"manager": "cargo", "name": "serde"}]);
        for request in [
            json!({"packages": packages}),
            json!({"packages": packages, "scope": "local"}),
        ] {
            let error = service
                .get_symbol(&lookup(&request)?)
                .expect_err("a local read names no package");
            assert_eq!(
                packages_violation(&error),
                Some("the local scope reads the project alone".to_owned()),
                "{request}: {error}"
            );
            assert_eq!(error.slug(), errors::server::read_invalid::SLUG);
        }
        Ok(())
    }

    /// Package facts follow the current tree alone, so `packages` beside `rev` refuses
    /// naming the field, whatever the scope.
    #[test]
    fn get_symbol_packages_beside_rev_refuses_naming_packages() -> TestResult {
        let (_directory, service) = beacon_fixture()?;
        for scope in ["local", "global", "all"] {
            let request = json!({
                "scope": scope,
                "rev": "main",
                "packages": [{"manager": "cargo", "name": "serde", "version": "1.0.228"}]
            });
            let error = service
                .get_symbol(&lookup(&request)?)
                .expect_err("a revision read names no package");
            assert_eq!(
                packages_violation(&error),
                Some("package facts are served for the current tree alone".to_owned()),
                "scope {scope}: {error}"
            );
        }
        Ok(())
    }

    /// The argument holds at most `REQUESTED_PACKAGES_MAX` entries, and the first entry
    /// outside its advertised lengths is named by position and rule.
    #[test]
    fn requested_packages_refuse_past_the_bound_and_name_a_malformed_entry() {
        let package = |name: &str| RequestedPackage {
            manager: "cargo".to_owned(),
            name: name.to_owned(),
            version: None,
        };
        let at_bound = vec![package("serde"); REQUESTED_PACKAGES_MAX];
        assert!(validate_requested_packages(SearchScope::All, false, &at_bound).is_ok());
        assert!(validate_requested_packages(SearchScope::Local, true, &[]).is_ok());

        let past_bound = vec![package("serde"); REQUESTED_PACKAGES_MAX + 1];
        let error = validate_requested_packages(SearchScope::Global, false, &past_bound)
            .expect_err("one entry past the bound refuses");
        assert_eq!(
            packages_violation(&error),
            Some("65 entries exceed the maximum 64".to_owned())
        );

        let error = validate_requested_packages(
            SearchScope::All,
            false,
            &[package("serde"), package(""), package("")],
        )
        .expect_err("an empty name refuses");
        assert_eq!(
            packages_violation(&error),
            Some("entry 1 breaks name_length".to_owned())
        );
    }

    /// A read naming packages resolves a context of its own, and one naming none reads
    /// the snapshot's context itself.
    #[test]
    fn read_context_applies_the_requested_packages_for_one_read() -> TestResult {
        let (_directory, service) = beacon_fixture()?;
        let unchanged = service.read_context(SearchScope::Global, None, &[])?;
        assert!(Arc::ptr_eq(&unchanged, service.dependency_context()));

        let requested = [RequestedPackage {
            manager: "cargo".to_owned(),
            name: "serde".to_owned(),
            version: Some("1.0.228".to_owned()),
        }];
        let held = service.dependency_context().entries().to_vec();
        let mut expected = held.clone();
        expected.push(requested[0].context_entry());
        expected.sort();
        let read = service.read_context(SearchScope::All, None, &requested)?;
        assert_eq!(read.entries(), expected);
        assert_eq!(service.dependency_context().entries(), held);

        let error = service
            .read_context(SearchScope::Local, None, &requested)
            .expect_err("a local read names no package");
        assert!(packages_violation(&error).is_some(), "{error}");
        Ok(())
    }

    #[test]
    fn documentation_error_keeps_its_identity_and_evidence() {
        let candidate = rift_index::DocumentationCollection::from_candidate_blocks(
            rift_protocol::read::Digest("00000000".to_owned()),
            Vec::new(),
            Vec::new(),
        )
        .expect("stored documentation revision");
        let failure = rift_index::DocumentationCollection::new(candidate.index().clone())
            .expect_err("incompatible documentation revision");
        let error = failure;

        assert_eq!(
            error.slug(),
            rift_error::errors::analysis::documentation_revision_invalid::SLUG
        );
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "field" && value == "documentation_revision" })
        );
        assert!(std::error::Error::source(&error).is_none());
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
        let error = errors::server::read_storage()
            .path("src/lib.rs")
            .operation("stage")
            .io(&io)
            .error();
        assert_eq!(error.slug(), errors::server::read_storage::SLUG);
        let context = error.context().collect::<Vec<_>>();
        assert!(context.contains(&("path", "src/lib.rs".to_owned())));
        assert!(context.contains(&("operation", "stage".to_owned())));
        assert!(context.contains(&("io", "sealed".to_owned())));
    }

    #[test]
    fn task_fault_is_internal_and_names_the_blocking_operation() {
        let error = errors::server::read_task()
            .operation("initial index build")
            .detail("worker panicked")
            .error();
        assert_eq!(error.slug(), errors::server::read_task::SLUG);
        let context = error.context().collect::<Vec<_>>();
        assert!(context.contains(&("operation", "initial index build".to_owned())));
        assert!(context.contains(&("detail", "worker panicked".to_owned())));
    }

    /// Every internal `SourceLocation` variant maps to its wire `SourceLocationKind`.
    #[test]
    fn wire_source_location_kind_maps_every_internal_variant() {
        let package = || rift_protocol::read::PackageIdentity {
            manager: "cargo".to_owned(),
            name: "beacon-core".to_owned(),
            version: "0.1.0".to_owned(),
        };
        let cases = [
            (
                rift_core::SourceLocation::Project { package: None },
                rift_protocol::read::SourceLocationKind::Project,
            ),
            (
                rift_core::SourceLocation::Dependency { package: package() },
                rift_protocol::read::SourceLocationKind::Dependency,
            ),
            (
                rift_core::SourceLocation::Stdlib {},
                rift_protocol::read::SourceLocationKind::Stdlib,
            ),
            (
                rift_core::SourceLocation::External {},
                rift_protocol::read::SourceLocationKind::External,
            ),
        ];
        for (internal, wire) in cases {
            assert_eq!(super::wire_source_location_kind(&internal), wire);
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
            .find(|node| node["kind"] == "struct_item")
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
            .find(|node| node["kind"] == "struct_item")
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
            .find(|node| node["kind"] == "impl_item")
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
        assert_eq!(symbol["language"], json!("typescript"));
        assert_eq!(symbol["kind"], json!("interface"));
        assert_eq!(symbol["facets"], json!(["type", "public"]));
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/typescript/src/routes.ts/Route")
        );
        assert_eq!(
            value["hits"][0]["source"], "export interface Route {\n  path: string;\n}",
            "the source starts at the `export` statement carrying the declaration"
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
            serde_json::from_value(json!({"name": "beacon", "language": "rust"}))?;
        let result = service.get_symbol(&filtered)?;
        let value = serde_json::to_value(&result)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(value["hits"][0]["symbol"]["language"], json!("rust"));
        assert_eq!(
            value["pagination"],
            json!({"page_index": 0, "total_pages": 1})
        );
        let dialect_filtered: GetSymbolParams = serde_json::from_value(json!({
            "name": "beacon",
            "language": "typescript:tsx"
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
            "language": "typescript"
        }))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        assert_eq!(
            value["hits"][0]["symbol"]["language"],
            json!("typescript:tsx")
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
                node["language"]
                    .as_str()
                    .is_some_and(|language| language.starts_with("typescript"))
            }),
            "every node carries the typescript language: {nodes:#?}"
        );
        assert!(
            nodes.iter().any(|node| node["kind"] == "jsx_element"),
            "position sits inside the JSX element: {nodes:#?}"
        );
        let jsx = nodes
            .iter()
            .find(|node| node["kind"] == "jsx_element")
            .ok_or("fixture must witness the jsx_element node")?;
        assert_eq!(jsx["language"], json!("typescript:tsx"));
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
            serde_json::from_value(json!({"name": "lookup", "include": ["history"]}))?;
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
    /// provider's kind word, empty facets, an id escaping the heading text,
    /// and the whole section as the source excerpt.
    #[test]
    fn get_symbol_finds_a_markdown_heading_beside_other_languages() -> TestResult {
        let (_directory, service) = multi_language_fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon Guide"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let symbol = &value["hits"][0]["symbol"];
        assert_eq!(symbol["language"], json!("markdown"));
        assert_eq!(symbol["kind"], json!("heading"));
        assert!(
            symbol.get("facets").is_none(),
            "no facets must omit the member"
        );
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/markdown/src/guide.md/Beacon%20Guide")
        );
        assert_eq!(
            value["hits"][0]["source"],
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
            nodes.iter().all(|node| node["language"] == "markdown"),
            "every node carries the markdown language: {nodes:#?}"
        );
        let section = nodes
            .iter()
            .find(|node| node["kind"] == "section")
            .ok_or("position sits inside the heading's section")?;
        assert_eq!(section["language"], json!("markdown"));
        assert_eq!(section["facets"], json!(["declaration"]));
        let section_id = section["id"].as_str().unwrap_or_default();
        assert!(
            section_id.starts_with("rift://node/markdown/"),
            "a markdown node address files under the markdown segment: {section_id}"
        );
        assert!(
            nodes.iter().any(|node| node["kind"] == "paragraph"),
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
        let request = json!({"name": "Install", "include": ["history"]});
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
    /// provider's kind word, empty facets, an id escaping the key, and the
    /// whole pair as the source excerpt.
    #[test]
    fn get_symbol_finds_a_json_member_beside_other_languages() -> TestResult {
        let (_directory, service) = multi_language_fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon settings"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let symbol = &value["hits"][0]["symbol"];
        assert_eq!(symbol["language"], json!("json"));
        assert_eq!(symbol["kind"], json!("member"));
        assert!(
            symbol.get("facets").is_none(),
            "no facets must omit the member"
        );
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/json/settings.json/beacon%20settings")
        );
        assert_eq!(
            value["hits"][0]["source"],
            "\"beacon settings\": {\"port\": 8080}"
        );

        let nested: GetSymbolParams = serde_json::from_value(json!({
            "name": "port",
            "language": "json"
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
        assert_eq!(symbol["language"], json!("yaml"));
        assert_eq!(symbol["kind"], json!("mapping_entry"));
        assert!(
            symbol.get("facets").is_none(),
            "no facets must omit the member"
        );
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/yaml/pipeline.yaml/beacon%20pipeline")
        );
        assert_eq!(
            value["hits"][0]["source"], "beacon pipeline:\n  retries: 3\n",
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
            nodes.iter().all(|node| node["language"] == "json"),
            "every node carries the json language: {nodes:#?}"
        );
        let pair = nodes
            .iter()
            .find(|node| node["kind"] == "pair")
            .ok_or("position sits inside the port member's pair")?;
        assert_eq!(pair["language"], json!("json"));
        assert_eq!(pair["facets"], json!(["declaration"]));
        let pair_id = pair["id"].as_str().unwrap_or_default();
        assert!(
            pair_id.starts_with("rift://node/json/"),
            "a JSON node address files under the json segment: {pair_id}"
        );
        assert!(
            nodes.iter().any(|node| node["kind"] == "number"),
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
            nodes.iter().all(|node| node["language"] == "yaml"),
            "every node carries the yaml language: {nodes:#?}"
        );
        let pair = nodes
            .iter()
            .find(|node| node["kind"] == "block_mapping_pair")
            .ok_or("position sits inside the retries entry's pair")?;
        assert_eq!(pair["language"], json!("yaml"));
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
        let request = json!({"name": "server", "include": ["history"]});
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
        let request = json!({"name": "retries", "include": ["history"]});
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
    ) -> Result<ReadService, super::RiftError> {
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

        let digests = service
            .visible_workspace_digests()
            .expect_err("a revision snapshot has no filesystem tree to digest");

        assert_eq!(digests.slug(), rift_error::errors::server::read_task::SLUG);
        assert!(digests.context().any(|(key, value)| {
            key == "operation" && value == "capture visible workspace digests"
        }));
        Ok(())
    }

    /// A revision snapshot has no filesystem tree to read changed paths from, so an
    /// incremental rebuild refuses before it reads one, naming the operation.
    #[test]
    fn a_revision_snapshot_refuses_an_incremental_rebuild() -> TestResult {
        let directory = committed_fixture()?;
        let service = revision_service(directory.path(), "main")?;
        let path = rift_core::ProjectPath::new("src/lib.rs")?;
        let edited = rift_index::FileDigest::of(b"pub fn beacon() -> u8 {\n    7\n}\n");
        let before = rift_index::WorkspaceDigests::new([]);
        let after = rift_index::WorkspaceDigests::new([(path, edited)]);

        let error = service
            .rebuilt(&rift_index::PathChanges::between(&before, &after))
            .expect_err("a revision snapshot has no filesystem tree to rebuild from");

        let context = error.context().collect::<Vec<_>>();
        assert_eq!(error.slug(), errors::server::read_task::SLUG);
        assert!(
            context
                .iter()
                .any(|(key, value)| { *key == "operation" && value == "incremental rebuild" })
        );
        assert!(context.iter().any(|(key, value)| {
            *key == "detail" && value == "a revision snapshot has no filesystem tree"
        }));
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
        assert_eq!(error.slug(), errors::server::read_unclaimed_extension::SLUG);
        assert_eq!(
            error
                .context()
                .find(|(key, _)| *key == "extension")
                .map(|(_, value)| value),
            Some("lock files".to_owned()),
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
            value["hits"][0]["source"], "pub fn beacon() {}",
            "the committed body answers, not the drifted working tree"
        );
        assert!(
            value.get("warnings").is_none(),
            "a live get_symbol result must omit warnings when there is nothing to warn about"
        );
        Ok(())
    }

    /// An ancestry suffix resolves through git's own revision syntax, and the snapshot
    /// names the commit it resolved to rather than the spelling it was asked with.
    #[test]
    fn revision_read_resolves_an_ancestry_suffix_to_its_commit() -> TestResult {
        let directory = committed_fixture()?;
        rift_history::fixture::commit_all(directory.path(), "return seven");
        let eight = "pub fn beacon() -> u8 {\n    8\n}\n";
        fs::write(directory.path().join("src/lib.rs"), eight)?;
        rift_history::fixture::commit_all(directory.path(), "return eight");
        let first = rift_history::Repository::open(directory.path())?
            .resolve("main~2")?
            .commit_id();
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;

        for spelling in ["HEAD~2", "HEAD^^", "main^1~1"] {
            let service = revision_service(directory.path(), spelling)?;
            assert_eq!(
                service.revision(),
                Some(&RevisionId(first.clone())),
                "{spelling}"
            );
            let value = serde_json::to_value(service.get_symbol(&params)?)?;
            assert_eq!(
                value["hits"][0]["source"], "pub fn beacon() {}",
                "{spelling}"
            );
        }
        Ok(())
    }

    /// A committed file the syntax provider refuses under its depth bound is left out of
    /// the revision index the way the workspace scan leaves it out, so `get_symbol` at
    /// the revision still answers from the file beside it and names the refused one.
    #[test]
    fn revision_read_leaves_out_a_file_past_a_syntax_bound_and_serves_the_rest() -> TestResult {
        let directory = committed_fixture()?;
        let deep = format!(
            "pub fn deep() -> i32 {{ {open}1{close} }}\n",
            open = "(".repeat(600),
            close = ")".repeat(600),
        );
        fs::write(directory.path().join("src/deep.rs"), deep)?;
        rift_history::fixture::commit_all(
            directory.path(),
            "commit a source past the syntax depth bound",
        );
        let service = revision_service(directory.path(), "HEAD")?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "beacon", "rev": "HEAD"}))?;
        let result = service.get_symbol(&params)?;
        assert_eq!(
            result.hits.len(),
            1,
            "the committed declaration beside the refused file answers"
        );
        let deep_path = rift_core::ProjectPath::new("src/deep.rs")?;
        assert!(
            result.warnings.iter().any(|warning| matches!(
                warning,
                ReadWarning::SourceUnavailable { unit: Some(unit), .. } if *unit == file_id(&deep_path)
            )),
            "the answer names the refused file: {:?}",
            result.warnings
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
            nodes.iter().any(|node| node["kind"] == "function_item"),
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
        assert_eq!(error.slug(), errors::history::unversioned::SLUG);
        let context = error.context().collect::<Vec<_>>();
        let canonical = fs::canonicalize(directory.path())?;
        assert!(context.contains(&("workspace", canonical.display().to_string())));
        assert!(
            context
                .iter()
                .any(|(key, value)| { *key == "requires" && value.contains("run `git init`") })
        );
        assert_eq!(
            error.to_string(),
            format!(
                "workspace has no git repository: {}: \
                 requires a git repository - run `git init`, or omit `rev` to \
                 read the current tree; run `git init`, or omit `rev` to read current tree",
                canonical.display()
            )
        );
        Ok(())
    }

    /// An engine answer the served bytes cannot carry is a condition of the
    /// tree, not a failure of the server: it classifies as content the caller
    /// cannot have, and no resend of the same request clears it.
    #[test]
    fn an_unmappable_engine_answer_classifies_as_content_the_request_cannot_have() {
        let error = errors::server::read_engine_answer()
            .operation("engine references")
            .detail("character out of range")
            .error();
        assert_eq!(error.slug(), errors::server::read_engine_answer::SLUG);
        assert_eq!(
            error.to_string(),
            "the addressed content exists but its bytes cannot be served: \
             operation engine references, detail character out of range; read \
             the request again once the engine has read the served revision"
        );
        assert!(
            Error::source(&error).is_none(),
            "the fault carries the engine's account, not the engine's failure"
        );
    }

    #[test]
    fn revision_read_refuses_an_unknown_revision_as_not_found() -> TestResult {
        let directory = committed_fixture()?;
        let error = revision_service(directory.path(), "feature/absent")
            .expect_err("an unknown revision must refuse");
        assert_eq!(error.slug(), errors::history::revision_unknown::SLUG);
        let context = error.context().collect::<Vec<_>>();
        assert!(context.contains(&("rev", "feature/absent".to_owned())));
        assert!(context.iter().any(|(key, value)| {
            *key == "requires" && value.contains("branch, tag, or commit id")
        }));
        Ok(())
    }

    #[test]
    fn revision_read_refuses_a_forbidden_spelling_as_invalid() -> TestResult {
        let directory = committed_fixture()?;
        let error = revision_service(directory.path(), "HEAD@{1}")
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

    /// `limit` at the advertised maximum is accepted; one over is refused naming the field
    /// and the maximum.
    #[test]
    fn accepted_limit_admits_the_maximum_and_refuses_one_over() {
        assert_eq!(
            accepted_limit(PAGE_LIMIT_MAX).ok(),
            usize::try_from(PAGE_LIMIT_MAX).ok()
        );
        let error =
            accepted_limit(PAGE_LIMIT_MAX + 1).expect_err("one over the maximum must refuse");
        assert_eq!(error.slug(), rift_error::errors::server::read_invalid::SLUG);
        assert_eq!(
            error.to_string(),
            "the request does not match the documented form: field limit, violation 10001 \
             exceeds the maximum 10000; correct the reported field and resend the request"
        );
    }

    /// Each index's match set is collected up to `results_max`; a set that reached it warns
    /// `results_truncated` naming the bound, and `total_pages` counts only what fit.
    #[test]
    fn get_symbol_at_the_result_bound_warns_results_truncated() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub struct Beacon;\nimpl Beacon { pub fn signal(&self) {} }\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::new(10, 4_096, 8_192, 8, 1)?,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "Beacon", "limit": 5}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        assert_eq!(value["hits"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            value["pagination"],
            json!({ "page_index": 0, "total_pages": 1 })
        );
        assert!(
            value["warnings"].as_array().is_some_and(|warnings| {
                warnings.contains(&json!({ "code": "results_truncated", "results_max": 1 }))
            }),
            "{value:#}"
        );
        Ok(())
    }

    /// A match set under `results_max` carries no `results_truncated` warning.
    #[test]
    fn get_symbol_under_the_result_bound_carries_no_results_truncated_warning() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let result = service.get_symbol(&params)?;
        assert!(
            !result
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::ResultsTruncated { .. })),
            "{:?}",
            result.warnings
        );
        Ok(())
    }

    /// The wire kind is the provider's own kind word alone: `language` rides beside
    /// `kind` in every payload, so the wire kind carries no language prefix.
    #[test]
    fn wire_kind_carries_the_provider_kind_word_unprefixed() {
        assert_eq!(super::wire_kind("function_item").0, "function_item");
        assert_eq!(super::wire_kind("class").0, "class");
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

    /// One contribution for the disagreement fixtures, anchored when `identity` is given.
    fn disagreement_contribution(
        provider_id: &str,
        symbol: &str,
        name: &str,
        identity: Option<&str>,
        equivalent_to: Option<(&str, &str)>,
    ) -> rift_core::Contribution {
        let facts = rift_core::PortableSymbolFacts::new(
            rift_core::Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            name,
            format!("crate::{name}"),
            rift_core::ExactKind("function".to_owned()),
        );
        let mut builder = rift_core::Contribution::builder(
            rift_core::ContributionKey::new(
                rift_core::ProviderId::new(provider_id).expect("provider"),
                rift_core::ProviderRevision::new(1).expect("revision"),
                rift_core::ProviderSymbolId::new(symbol).expect("provider symbol"),
            ),
            rift_core::SourceApplicability::Independent,
            facts,
            rift_core::ContributionOrigin::new(
                Some(rift_core::SourceLocation::Project { package: None }),
                rift_core::SourceKind::Authored,
            )
            .expect("origin"),
        );
        if let Some(identity) = identity {
            builder = builder
                .source(rift_core::DeclarationBinding::new(
                    rift_core::SourceUnitId::parse("rift://source/rift.sources.project/src/lib.rs")
                        .expect("source unit"),
                    rift_core::SourceRange::new(0, 8).expect("range"),
                    None,
                ))
                .identity_anchor(rift_core::SymbolId::new(identity).expect("identity"));
        }
        if let Some((provider_id, symbol)) = equivalent_to {
            builder = builder.equivalence(vec![rift_core::EquivalenceEvidence::Explicit(
                rift_core::ContributionReference::new(
                    rift_core::ProviderId::new(provider_id).expect("provider"),
                    rift_core::ProviderSymbolId::new(symbol).expect("provider symbol"),
                ),
            )]);
        }
        builder.build().expect("contribution")
    }

    /// Normalizes one two-provider graph whose contributions disagree on the name.
    fn disagreeing_assembly(identity: Option<&str>) -> rift_provider::AssembledSymbol {
        let limits = rift_provider::PublicationLimits::default();
        let syntax = rift_provider::ProviderPublication::new(
            rift_core::ProviderId::new("syntax").expect("provider"),
            rift_core::ProviderRevision::new(1).expect("revision"),
            vec![disagreement_contribution(
                "syntax",
                "syntax-beacon",
                "Beacon",
                identity,
                None,
            )],
            limits,
        )
        .expect("syntax publication");
        let binding = rift_provider::ProviderPublication::new(
            rift_core::ProviderId::new("binding").expect("provider"),
            rift_core::ProviderRevision::new(1).expect("revision"),
            vec![disagreement_contribution(
                "binding",
                "binding-beacon",
                "beacon",
                None,
                Some(("syntax", "syntax-beacon")),
            )],
            limits,
        )
        .expect("binding publication");
        let set = rift_provider::PublicationSet::empty(limits)
            .replaced(syntax)
            .and_then(|set| set.replaced(binding))
            .expect("publications");
        let graph = rift_provider::Normalizer::normalize(
            rift_core::IndexRevision::new(1).expect("index"),
            rift_core::SourceRevision::new(1).expect("source"),
            rift_core::TreeRevision::new(1).expect("tree"),
            &std::sync::Arc::new(set),
            None,
        )
        .expect("graph");
        let record = graph
            .records()
            .iter()
            .find(|record| record.contributions().len() == 2)
            .expect("merged record")
            .clone();
        rift_provider::SymbolAssembler::assemble(
            &graph,
            &record,
            &[rift_core::ProviderId::new("syntax").expect("provider")],
        )
        .expect("assembled symbol")
    }

    #[test]
    fn a_retained_disagreement_becomes_one_symbol_disagreement_warning() {
        let assembled = disagreeing_assembly(Some("symbol:beacon"));
        assert!(
            !assembled.disagreements().is_empty(),
            "the two-provider fixture must disagree on the name"
        );
        let warning = super::symbol_disagreement_warning(&assembled)
            .expect("an established disagreeing symbol warns");
        let ReadWarning::SymbolDisagreement {
            symbol,
            providers,
            detail,
        } = warning
        else {
            panic!("the warning must carry the symbol_disagreement code");
        };
        assert_eq!(symbol.0, "symbol:beacon");
        assert_eq!(
            providers,
            ["binding"],
            "the warning names the providers whose presentation lost the selection"
        );
        assert!(detail.contains("binding"), "{detail}");
    }

    #[test]
    fn a_disagreement_without_an_established_identity_stays_silent() {
        let assembled = disagreeing_assembly(None);
        assert!(!assembled.disagreements().is_empty());
        assert_eq!(super::symbol_disagreement_warning(&assembled), None);
    }

    const GIT_TIMEOUT: Duration = Duration::from_secs(10);
    const GIT_CAPTURE_BYTES: usize = 32 << 10;

    fn bounded_git(
        root: &Path,
        global_config: &Path,
        options: &[&str],
        arguments: &[&str],
    ) -> TestResult<String> {
        let mut command = crate::process::Command::new("git");
        command
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "Rift Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@rift.invalid")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00 +0000")
            .env("GIT_COMMITTER_NAME", "Rift Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@rift.invalid")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00 +0000")
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
            ]);
        for option in options {
            command.args(["-c", option]);
        }
        command.args(arguments);

        let run =
            crate::process::run_bounded(&mut command, GIT_TIMEOUT, GIT_CAPTURE_BYTES, &|| false)?;
        assert_eq!(
            run.ending,
            crate::process::RunEnding::Exited,
            "git {arguments:?} exceeded {GIT_TIMEOUT:?}"
        );
        assert!(
            !run.stdout.truncated && !run.stderr.truncated,
            "git {:?} output exceeded {GIT_CAPTURE_BYTES} bytes: stdout={:?}, stderr={:?}",
            arguments,
            run.stdout,
            run.stderr
        );
        let status = run.exit?;
        assert!(
            status.success(),
            "git {:?} failed with {status}: stdout={:?}, stderr={:?}",
            arguments,
            run.stdout.text,
            run.stderr.text
        );
        Ok(run.stdout.text)
    }

    fn set_modified(path: &Path, modified: SystemTime) -> TestResult {
        OpenOptions::new()
            .write(true)
            .open(path)?
            .set_modified(modified)?;
        assert_eq!(
            fs::metadata(path)?.modified()?,
            modified,
            "filesystem retains exact requested modification time for {}",
            path.display()
        );
        Ok(())
    }

    fn git_cached_modified(root: &Path, global_config: &Path) -> TestResult<SystemTime> {
        let debug = bounded_git(
            root,
            global_config,
            &[],
            &["ls-files", "--debug", "--", "source.rs"],
        )?;
        let value = debug
            .lines()
            .find_map(|line| line.trim().strip_prefix("mtime:"))
            .ok_or("git ls-files --debug omitted cached mtime")?
            .trim();
        let (seconds, nanoseconds) = value
            .split_once(':')
            .ok_or("git ls-files --debug returned malformed cached mtime")?;
        let modified = UNIX_EPOCH
            .checked_add(Duration::new(seconds.parse()?, nanoseconds.parse()?))
            .ok_or("cached mtime is outside SystemTime range")?;
        Ok(modified)
    }

    fn paired_capture(
        root: &Path,
        last: &LastCapture,
        git_status: &str,
        case: &str,
    ) -> TestResult<(rift_index::WorkspaceDigests, LastCapture)> {
        let (captured, next) = capture_digests_with_languages(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            last,
        )?;
        let cold = WorkspaceIndex::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        assert_eq!(
            captured,
            cold.digests(),
            "incremental capture must match cold build after {case}; Git status={git_status:?}"
        );
        Ok((captured, next))
    }

    fn stage_source(root: &Path, global_config: &Path) -> TestResult {
        bounded_git(root, global_config, &[], &["add", "-A"])?;
        Ok(())
    }

    fn commit_staged(root: &Path, global_config: &Path, allow_empty: bool) -> TestResult {
        let mut arguments = vec!["commit", "--quiet", "-m", "fixture"];
        if allow_empty {
            arguments.push("--allow-empty");
        }
        bounded_git(root, global_config, &[], &arguments)?;
        Ok(())
    }

    fn commit_source(root: &Path, global_config: &Path, allow_empty: bool) -> TestResult {
        stage_source(root, global_config)?;
        commit_staged(root, global_config, allow_empty)
    }

    fn check_ordinary_git_edit(
        root: &Path,
        global_config: &Path,
        source: &Path,
        previous: &mut rift_index::WorkspaceDigests,
        last: &mut LastCapture,
    ) -> TestResult {
        let same_size = fs::metadata(source)?.len();
        fs::write(source, b"fn bravo() {}\n")?;
        assert_eq!(
            fs::metadata(source)?.len(),
            same_size,
            "ordinary edit retains file size"
        );
        let ordinary_status = bounded_git(
            root,
            global_config,
            &[],
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        assert!(
            !ordinary_status.is_empty(),
            "ordinary edit must appear in Git status: {ordinary_status:?}"
        );
        let (captured, next) = paired_capture(root, last, &ordinary_status, "ordinary edit")?;
        assert_ne!(
            captured, *previous,
            "ordinary edit changes captured digests"
        );
        *previous = captured;
        *last = next;
        rift_tracing::info!(
            component = "index",
            git_status = ?ordinary_status,
            "paired Git status, ordinary edit"
        );
        commit_source(root, global_config, false)
    }

    fn check_racy_git_baseline(
        root: &Path,
        global_config: &Path,
        source: &Path,
        previous: &mut rift_index::WorkspaceDigests,
        last: &mut LastCapture,
    ) -> TestResult<SystemTime> {
        let racy_time = UNIX_EPOCH + Duration::from_secs(1_750_000_000);
        set_modified(source, racy_time)?;
        commit_source(root, global_config, true)?;
        assert_eq!(git_cached_modified(root, global_config)?, racy_time);
        let index = root.join(".git/index");
        set_modified(&index, racy_time)?;
        assert_eq!(fs::metadata(&index)?.modified()?, racy_time);
        let racy_options = ["core.trustctime=false"];
        let racy_baseline = bounded_git(
            root,
            global_config,
            &racy_options,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        assert!(
            racy_baseline.is_empty(),
            "racy Git baseline={racy_baseline:?}"
        );
        assert_eq!(
            git_cached_modified(root, global_config)?,
            racy_time,
            "Git cached mtime remains exact before racy rewrite"
        );
        assert_eq!(
            fs::metadata(source)?.modified()?,
            fs::metadata(&index)?.modified()?,
            "source mtime equals index mtime before racy rewrite"
        );
        let (baseline_capture, next) =
            paired_capture(root, last, &racy_baseline, "racy control baseline")?;
        assert_eq!(
            baseline_capture, *previous,
            "racy setup preserves file contents"
        );
        *previous = baseline_capture;
        *last = next;
        Ok(racy_time)
    }

    fn check_racy_git_rewrite(
        root: &Path,
        global_config: &Path,
        source: &Path,
        racy_time: SystemTime,
        previous: &mut rift_index::WorkspaceDigests,
        last: &mut LastCapture,
    ) -> TestResult {
        let index = root.join(".git/index");
        let racy_size = fs::metadata(source)?.len();
        fs::write(source, b"fn delta() {}\n")?;
        assert_eq!(
            fs::metadata(source)?.len(),
            racy_size,
            "racy rewrite retains file size"
        );
        set_modified(source, racy_time)?;
        assert_eq!(
            fs::metadata(source)?.modified()?,
            fs::metadata(&index)?.modified()?,
            "source mtime equals index mtime after racy rewrite"
        );
        assert_eq!(git_cached_modified(root, global_config)?, racy_time);
        let racy_options = ["core.trustctime=false"];
        let racy_status = bounded_git(
            root,
            global_config,
            &racy_options,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        assert!(
            !racy_status.is_empty(),
            "Git racy content check must report equal-stat rewrite: {racy_status:?}"
        );
        let (captured, next) = paired_capture(root, last, &racy_status, "racy rewrite")?;
        assert_ne!(captured, *previous, "racy rewrite changes captured digests");
        *previous = captured;
        *last = next;
        rift_tracing::info!(
            component = "index",
            git_status = ?racy_status,
            "paired Git status, racy rewrite"
        );
        commit_source(root, global_config, false)
    }

    fn check_old_mtime_baseline(
        root: &Path,
        global_config: &Path,
        source: &Path,
        old_time: SystemTime,
        previous: &mut rift_index::WorkspaceDigests,
        last: &mut LastCapture,
    ) -> TestResult {
        set_modified(source, old_time)?;
        commit_source(root, global_config, true)?;
        let index = root.join(".git/index");
        let old_mtime_index_time = fs::metadata(&index)?.modified()?;
        assert!(old_time < old_mtime_index_time);
        assert_eq!(git_cached_modified(root, global_config)?, old_time);
        let old_baseline = bounded_git(
            root,
            global_config,
            &[],
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        assert!(
            old_baseline.is_empty(),
            "old-mtime baseline={old_baseline:?}"
        );
        let (baseline_capture, next) =
            paired_capture(root, last, &old_baseline, "old-mtime baseline")?;
        assert_eq!(
            baseline_capture, *previous,
            "old-mtime setup preserves file contents"
        );
        *previous = baseline_capture;
        *last = next;
        Ok(())
    }

    fn check_restored_old_mtime_rewrite(
        root: &Path,
        global_config: &Path,
        source: &Path,
        old_time: SystemTime,
        previous: &mut rift_index::WorkspaceDigests,
        last: &mut LastCapture,
    ) -> TestResult {
        let old_mtime_size = fs::metadata(source)?.len();
        fs::write(source, b"fn gamma() {}\n")?;
        assert_eq!(
            fs::metadata(source)?.len(),
            old_mtime_size,
            "old-mtime rewrite retains file size"
        );
        set_modified(source, old_time)?;
        let old_mtime_status = bounded_git(
            root,
            global_config,
            &[],
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        assert_eq!(fs::metadata(source)?.modified()?, old_time);
        assert_eq!(git_cached_modified(root, global_config)?, old_time);
        let (captured, next) = paired_capture(root, last, &old_mtime_status, "restored old mtime")?;
        assert_ne!(
            captured, *previous,
            "restored old-mtime rewrite changes captured digests"
        );
        *previous = captured;
        *last = next;
        rift_tracing::info!(
            component = "index",
            git_status = ?old_mtime_status,
            "paired Git status, restored old mtime"
        );
        Ok(())
    }

    fn repair_old_mtime_git_status(
        root: &Path,
        global_config: &Path,
        source: &Path,
        old_time: SystemTime,
    ) -> TestResult {
        let refreshed_time = old_time + Duration::from_secs(1);
        set_modified(source, refreshed_time)?;
        assert_ne!(
            fs::metadata(source)?.modified()?,
            git_cached_modified(root, global_config)?,
            "commit repair must move source mtime past stale cached stat"
        );
        stage_source(root, global_config)?;
        let staged_status = bounded_git(
            root,
            global_config,
            &[],
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        assert!(
            !staged_status.is_empty(),
            "Git stages changed bytes after mtime moves past cached stat: {staged_status:?}"
        );
        commit_staged(root, global_config, false)?;
        let repaired_status = bounded_git(
            root,
            global_config,
            &[],
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        assert!(
            repaired_status.is_empty(),
            "Git status after old-mtime fixture repair={repaired_status:?}"
        );
        Ok(())
    }

    fn check_case_only_rename(
        root: &Path,
        global_config: &Path,
        source: &Path,
        previous: &mut rift_index::WorkspaceDigests,
        last: &mut LastCapture,
    ) -> TestResult {
        let (baseline_capture, next) = paired_capture(root, last, "", "case-rename baseline")?;
        assert_eq!(
            baseline_capture, *previous,
            "commit preserves capture contents"
        );
        *previous = baseline_capture;
        *last = next;
        let temporary_name = root.join("temporary.rs");
        let case_name = root.join("Source.rs");
        fs::rename(source, &temporary_name)?;
        fs::rename(&temporary_name, &case_name)?;
        let case_status = bounded_git(
            root,
            global_config,
            &[],
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        let (captured, _) = paired_capture(root, last, &case_status, "case-only rename")?;
        assert_ne!(
            captured, *previous,
            "case-only rename changes captured digests"
        );
        rift_tracing::info!(
            component = "index",
            git_status = ?case_status,
            "paired Git status, case-only rename"
        );
        Ok(())
    }

    #[test]
    fn retained_capture_matches_git_and_cold_build_across_file_mutations() -> TestResult {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("workspace");
        fs::create_dir(&root)?;
        let global_config = temporary.path().join("empty.gitconfig");
        fs::write(&global_config, "")?;
        let source = root.join("source.rs");
        fs::write(&source, b"fn alpha() {}\n")?;
        bounded_git(&root, &global_config, &[], &["init", "-q", "-b", "main"])?;
        fs::write(root.join(".git/info/exclude"), ".rift/\n")?;
        commit_source(&root, &global_config, false)?;
        let baseline_status = bounded_git(
            &root,
            &global_config,
            &[],
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        assert!(
            baseline_status.is_empty(),
            "initial Git status={baseline_status:?}"
        );
        let (mut previous, mut last) = paired_capture(
            &root,
            &LastCapture::default(),
            &baseline_status,
            "initial capture",
        )?;

        check_ordinary_git_edit(&root, &global_config, &source, &mut previous, &mut last)?;
        let racy_time =
            check_racy_git_baseline(&root, &global_config, &source, &mut previous, &mut last)?;
        check_racy_git_rewrite(
            &root,
            &global_config,
            &source,
            racy_time,
            &mut previous,
            &mut last,
        )?;
        let old_time = UNIX_EPOCH + Duration::from_hours(438_288);
        check_old_mtime_baseline(
            &root,
            &global_config,
            &source,
            old_time,
            &mut previous,
            &mut last,
        )?;
        check_restored_old_mtime_rewrite(
            &root,
            &global_config,
            &source,
            old_time,
            &mut previous,
            &mut last,
        )?;
        repair_old_mtime_git_status(&root, &global_config, &source, old_time)?;
        check_case_only_rename(&root, &global_config, &source, &mut previous, &mut last)
    }
}
