//! Symbol timelines: version-control history read through the syntax tier.
//!
//! A workspace whose server keeps a history store answers a timeline from
//! it: the store holds, per analyzed commit, the declarations whose shape
//! changed and the files moved without a byte changed, and the timeline
//! follows the commits it holds from the served revision. With no store
//! attached, one request's composition walks each hit path's first-parent
//! history, parses the committed blobs with the path's syntax provider, and
//! classifies each adjacent pair of parsed states into a wire
//! [`SymbolVersionKind`]. The caller runs either on its blocking lane; the
//! classifier itself is sans-I/O.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rift_core::ProjectPath;
use rift_history::{
    HistoryFault, PathHistory, PathRevision, Repository, ResolvedRevision, TreeFile,
};
use rift_history_store::{StoreReader, StoreReads, StoredCommit};
use rift_index::SymbolMatch;
use rift_protocol::configuration::{HISTORY_REVISIONS_MAX, HistoryConfiguration, HistoryStrategy};
use rift_protocol::read::{
    CommitAuthor, ProjectPath as WireProjectPath, ReadWarning, RevisionId, SymbolHistory, SymbolId,
    SymbolVersion, SymbolVersionKind,
};
use rift_syntax::{SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource, SyntaxSymbol};

use crate::read::{ReadError, ReadFault, project_path, symbol_id};

/// One parse cache key: the path selects the provider and symbol space, the
/// blob id the exact committed bytes.
type ParseKey = (String, String);

/// One committed blob's parsed source: the text beside the document
/// extracted from it. `None` in the cache marks a blob the tier cannot
/// analyze.
#[derive(Debug)]
struct ParsedRevision {
    text: String,
    document: SyntaxDocument,
}

/// The history store a symbol-history read answers from, the signal that
/// asks the history task to fill when the store lags a read, and how far the
/// task's latest fill has got.
#[derive(Clone)]
pub struct StoredHistory {
    reader: StoreReader,
    strategy: HistoryStrategy,
    lagging: Arc<dyn Fn() + Send + Sync>,
    progress: Arc<FillProgress>,
}

impl StoredHistory {
    /// The store `reader` opens, filled under `strategy`. A read that meets
    /// a served revision the store does not hold yet calls `lagging` once
    /// and answers from what the store holds.
    #[must_use]
    pub fn new(
        reader: StoreReader,
        strategy: HistoryStrategy,
        lagging: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            reader,
            strategy,
            lagging,
            progress: Arc::default(),
        }
    }

    /// Opens one read connection to the store.
    pub(crate) fn connect(&self) -> Result<StoreReads, ReadError> {
        self.reader.connect().map_err(ReadFault::history_store)
    }

    /// How far the history task's latest fill has got, which the task records
    /// and a commit search reports.
    #[must_use]
    pub fn progress(&self) -> Arc<FillProgress> {
        Arc::clone(&self.progress)
    }

    /// The `history_store_filling` warning a commit search over `reads`
    /// carries, or `None` when the store holds every commit the latest fill
    /// selects and, under `everything`, the commit `HEAD` names in the
    /// workspace at `root`.
    ///
    /// A store missing `HEAD`'s commit was planned against an older head, so
    /// the read asks the history task to fill, as a lagging timeline does.
    /// Before the task's first plan lands there is nothing to compare with,
    /// and no warning.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when `HEAD` does not resolve or the store cannot
    /// be read.
    pub(crate) fn filling(
        &self,
        reads: &StoreReads,
        root: &Path,
    ) -> Result<Option<ReadWarning>, ReadError> {
        let Some(counts) = self.progress.counts() else {
            return Ok(None);
        };
        let head_missing = match self.strategy {
            HistoryStrategy::Everything => {
                let head = Repository::open(root)
                    .and_then(|repository| repository.resolve("HEAD"))
                    .map_err(ReadFault::history)?;
                reads
                    .commit(&head.commit_id())
                    .map_err(ReadFault::history_store)?
                    .is_none()
            }
            HistoryStrategy::Selective => false,
        };
        if head_missing {
            (self.lagging)();
        }
        Ok(counts.filling_warning(head_missing))
    }
}

/// How far the history task's latest fill has got: the commits its plan
/// selects and how many of them the store still lacks. The task records each
/// plan and each written batch; readers take a consistent copy.
#[derive(Debug, Default)]
pub struct FillProgress {
    latest: Mutex<Option<FillCounts>>,
}

impl FillProgress {
    /// Records one plan: it selects `selected` commits and the store lacks
    /// `owed` of them.
    pub fn record_plan(&self, selected: usize, owed: usize) {
        let counts = FillCounts {
            total: u64::try_from(selected).unwrap_or(u64::MAX),
            owed: u64::try_from(owed.min(selected)).unwrap_or(u64::MAX),
        };
        *self.lock() = Some(counts);
    }

    /// Records one written batch of `commits` commits the latest plan owed.
    pub fn record_written(&self, commits: usize) {
        if let Some(counts) = self.lock().as_mut() {
            counts.owed = counts
                .owed
                .saturating_sub(u64::try_from(commits).unwrap_or(u64::MAX));
        }
    }

    /// The latest plan's counts, or `None` before the first plan lands.
    #[must_use]
    pub fn counts(&self) -> Option<FillCounts> {
        *self.lock()
    }

    /// The counts, whatever a panicking holder left: each write replaces or
    /// decrements whole values, so no holder can leave them half-written.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<FillCounts>> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One plan's counts: the commits it selects and how many the store lacks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FillCounts {
    total: u64,
    owed: u64,
}

impl FillCounts {
    /// Commits the store holds of the ones the fill plan selects.
    #[must_use]
    pub const fn analyzed(self) -> u64 {
        self.total.saturating_sub(self.owed)
    }

    /// Commits the fill plan selects.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.total
    }

    /// The warning these counts call for: one while the store lacks a
    /// selected commit, or the served `HEAD` commit when `head_missing`.
    fn filling_warning(self, head_missing: bool) -> Option<ReadWarning> {
        let analyzed = self.analyzed();
        let detail = match (head_missing, self.owed) {
            (true, _) => format!(
                "the history store has not analyzed the commit HEAD names yet, and holds \
                 {analyzed} of the {} commits its latest fill selects, so a commit it has \
                 not reached answers nothing; the fill runs in the background, and a later \
                 search answers the rest",
                self.total
            ),
            (false, 0) => return None,
            (false, _) => format!(
                "the history store holds {analyzed} of the {} commits its latest fill \
                 selects, so a commit it has not reached answers nothing; the fill runs in \
                 the background, and a later search answers the rest",
                self.total
            ),
        };
        Some(ReadWarning::HistoryStoreFilling {
            analyzed,
            total: self.total,
            detail,
        })
    }
}

impl std::fmt::Debug for StoredHistory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredHistory")
            .field("reader", &self.reader)
            .field("strategy", &self.strategy)
            .finish_non_exhaustive()
    }
}

/// Per-request timeline composition over one served revision.
#[derive(Debug)]
pub(crate) struct SymbolTimelines {
    _span: tracing::Span,
    source: TimelineSource,
}

/// Where one request's timelines come from.
#[derive(Debug)]
enum TimelineSource {
    /// The history store the server fills.
    Store(StoredTimelines),
    /// A walk of git per request, when no store is attached.
    Walk(Box<WalkedTimelines>),
}

impl SymbolTimelines {
    /// Opens the workspace repository and resolves where timelines start:
    /// the served commit for a revision read, `HEAD` for a current-tree
    /// read. The walk budget is the configured history depth under the
    /// protocol's hard bound. With `store` attached, the timelines read it;
    /// otherwise each walks git.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`]: `unsupported` when `[providers.history]` is
    /// disabled, the version-control fault when the workspace has no
    /// repository or its head does not resolve, and the store fault when the
    /// attached store cannot be read.
    pub(crate) fn open(
        root: &Path,
        revision: Option<&RevisionId>,
        history: &HistoryConfiguration,
        syntax: SyntaxLimits,
        store: Option<&StoredHistory>,
    ) -> Result<Self, ReadError> {
        if !history.enabled {
            return Err(ReadFault::unsupported(
                "symbol history (providers.history disabled)",
            ));
        }
        let repository = Repository::open(root).map_err(ReadFault::history)?;
        let start = match revision {
            Some(revision) => repository.resolve(&revision.0),
            None => repository.resolve("HEAD"),
        }
        .map_err(ReadFault::history)?;
        let revisions_max =
            usize::try_from(history.max_revisions.min(HISTORY_REVISIONS_MAX)).unwrap_or(usize::MAX);
        let span = tracing::debug_span!(
            "get_symbol",
            component = "index",
            operation = "get_symbol",
            phase = "history"
        );
        tracing::debug!(
            component = "index",
            operation = "get_symbol",
            phase = "start",
            "symbol history started"
        );
        let source = match store {
            Some(stored) => TimelineSource::Store(StoredTimelines::open(
                stored,
                &start,
                revision.is_some(),
                revisions_max,
            )?),
            None => TimelineSource::Walk(Box::new(WalkedTimelines {
                repository,
                start,
                revisions_max,
                syntax,
                walks: HashMap::new(),
                parses: HashMap::new(),
            })),
        };
        Ok(Self {
            _span: span,
            source,
        })
    }

    /// Composes one hit's timeline, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the repository or the store cannot be
    /// read. A blob the tier cannot analyze contributes no version instead of
    /// failing.
    pub(crate) fn timeline(
        &mut self,
        provider: &dyn SyntaxProvider,
        matched: SymbolMatch<'_>,
    ) -> Result<SymbolHistory, ReadError> {
        match &mut self.source {
            TimelineSource::Store(stored) => stored.timeline(matched),
            TimelineSource::Walk(walked) => walked.timeline(provider, matched),
        }
    }
}

/// Timelines read from the history store: one read connection, and the
/// held commit they start at.
#[derive(Debug)]
struct StoredTimelines {
    reads: StoreReads,
    start: Option<String>,
    revisions_max: usize,
}

impl StoredTimelines {
    /// Opens one read connection and settles where the timelines start.
    ///
    /// Under `everything` they start at the served commit. Under `selective`
    /// a current-tree read starts at the newest release the store holds and
    /// a revision read at the served commit when it is a held release. A
    /// start the store does not hold answers no version, and a current-tree
    /// read that meets one asks the history task to fill.
    fn open(
        stored: &StoredHistory,
        served: &ResolvedRevision,
        revision_read: bool,
        revisions_max: usize,
    ) -> Result<Self, ReadError> {
        let reads = stored.connect()?;
        let start = match (stored.strategy, revision_read) {
            (HistoryStrategy::Selective, false) => {
                reads.chain_head().map_err(ReadFault::history_store)?
            }
            _ => Some(served.commit_id()),
        };
        let held = match &start {
            Some(start) => reads
                .commit(start)
                .map_err(ReadFault::history_store)?
                .is_some(),
            None => false,
        };
        if !held && !revision_read {
            (stored.lagging)();
        }
        Ok(Self {
            reads,
            start,
            revisions_max,
        })
    }

    /// One declaration's timeline through the held commits, newest first.
    ///
    /// Each held commit contributes the change the store recorded for the
    /// declaration at the path it lived at then. A file the commit moved
    /// without changing a byte, or a declaration it moved unchanged into
    /// another file, contributes a `moved` version, and the timeline goes on
    /// at the path it came from. The timeline is
    /// complete once it passes a commit compared with nothing; it is not
    /// when it meets a commit the store does not hold, a boundary, or the
    /// `max_revisions` bound first.
    fn timeline(&self, matched: SymbolMatch<'_>) -> Result<SymbolHistory, ReadError> {
        let symbol = symbol_id(matched.file, matched.symbol);
        let qualified_name = matched.symbol.qualified_name.as_str();
        let mut path = matched.file.path().as_str().to_owned();
        let mut versions = Vec::new();
        let mut next = self.start.clone();
        let mut complete = false;
        for _ in 0..self.revisions_max {
            let Some(id) = next else {
                complete = true;
                break;
            };
            let Some(commit) = self.reads.commit(&id).map_err(ReadFault::history_store)? else {
                break;
            };
            if let Some(kind) = self
                .reads
                .declaration_change(&commit, &path, qualified_name)
                .map_err(ReadFault::history_store)?
            {
                versions.push(stored_version(&commit, &path, kind));
                if kind == SymbolVersionKind::Moved
                    && let Some(old_path) = self
                        .reads
                        .moved_from(&commit, &path, qualified_name)
                        .map_err(ReadFault::history_store)?
                {
                    path = old_path;
                }
            }
            if let Some(old_path) = self
                .reads
                .renamed_from(&commit, &path)
                .map_err(ReadFault::history_store)?
            {
                versions.push(stored_version(&commit, &path, SymbolVersionKind::Moved));
                path = old_path;
            }
            if commit.boundary {
                break;
            }
            next = commit.base;
        }
        Ok(timeline_answer(symbol, versions, complete))
    }
}

/// One version a held commit contributes.
fn stored_version(commit: &StoredCommit, path: &str, kind: SymbolVersionKind) -> SymbolVersion {
    SymbolVersion {
        revision: RevisionId(commit.id.clone()),
        path: WireProjectPath(path.to_owned()),
        kind,
        timestamp: commit.committed_at.clone(),
        summary: commit.summary().map(str::to_owned),
        author: commit.author.clone(),
    }
}

/// The wire timeline one composition answers.
const fn timeline_answer(
    symbol: SymbolId,
    versions: Vec<SymbolVersion>,
    complete: bool,
) -> SymbolHistory {
    SymbolHistory {
        symbol,
        versions,
        complete,
    }
}

/// Timelines walked through git per request: one repository handle, one
/// walk per distinct hit path, one parse per distinct committed blob.
#[derive(Debug)]
struct WalkedTimelines {
    repository: Repository,
    start: ResolvedRevision,
    revisions_max: usize,
    syntax: SyntaxLimits,
    walks: HashMap<String, PathHistory>,
    parses: HashMap<ParseKey, Option<ParsedRevision>>,
}

impl WalkedTimelines {
    /// Composes one hit's timeline: the path's touching commits newest
    /// first, each parsed through `provider` and classified against its
    /// adjacent older state.
    fn timeline(
        &mut self,
        provider: &dyn SyntaxProvider,
        matched: SymbolMatch<'_>,
    ) -> Result<SymbolHistory, ReadError> {
        let path = matched.file.path();
        let Self {
            repository,
            start,
            revisions_max,
            syntax,
            walks,
            parses,
        } = self;
        let history = match walks.entry(path.as_str().to_owned()) {
            Entry::Occupied(walked) => walked.into_mut(),
            Entry::Vacant(unwalked) => unwalked.insert(
                repository
                    .path_revisions(start, path.as_str(), *revisions_max)
                    .map_err(ReadFault::history)?,
            ),
        };
        let mut states = Vec::with_capacity(history.revisions().len());
        for revision in history.revisions() {
            states.push(revision_state(
                repository,
                parses,
                provider,
                *syntax,
                path,
                revision,
                &matched.symbol.qualified_name,
            )?);
        }
        // The state past the oldest walked commit: provably absent when the
        // walk covered the path's whole history, unknown - contributing no
        // version - when the examination bound cut the walk short.
        let complete = history.is_complete();
        let boundary = if complete {
            SymbolState::Absent
        } else {
            SymbolState::Unknown
        };
        let mut versions = Vec::with_capacity(states.len());
        for (index, revision) in history.revisions().iter().enumerate() {
            let older = states.get(index + 1).unwrap_or(&boundary);
            let Some(kind) = classify(older, &states[index]) else {
                continue;
            };
            versions.push(SymbolVersion {
                revision: RevisionId(revision.commit_id().to_owned()),
                path: project_path(path),
                kind,
                timestamp: revision.timestamp().to_owned(),
                summary: revision.summary().map(str::to_owned),
                author: CommitAuthor {
                    name: revision.author_name().to_owned(),
                    email: revision.author_email().to_owned(),
                },
            });
        }
        Ok(timeline_answer(
            symbol_id(matched.file, matched.symbol),
            versions,
            complete,
        ))
    }
}

/// The declaration's state at one touching commit: parsed from the
/// committed blob, absent when the commit removed the file or the parse
/// lacks the symbol, unknown when the blob cannot be analyzed.
fn revision_state(
    repository: &Repository,
    parses: &mut HashMap<ParseKey, Option<ParsedRevision>>,
    provider: &dyn SyntaxProvider,
    syntax: SyntaxLimits,
    path: &ProjectPath,
    revision: &PathRevision,
    qualified_name: &str,
) -> Result<SymbolState, ReadError> {
    let Some(blob) = revision.blob() else {
        return Ok(SymbolState::Absent);
    };
    let key = (path.as_str().to_owned(), blob.blob_id());
    let cached = match parses.entry(key) {
        Entry::Occupied(occupied) => occupied.into_mut(),
        Entry::Vacant(vacant) => {
            vacant.insert(parse_blob(repository, provider, syntax, path, blob)?)
        }
    };
    Ok(match cached {
        Some(analysis) => analysis
            .document
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
            .map_or(SymbolState::Absent, |symbol| {
                SymbolState::Present(SymbolShape::from_source(&analysis.text, symbol))
            }),
        None => SymbolState::Unknown,
    })
}

/// Parses one committed blob through the provider under `syntax`. `None`
/// marks a blob the tier cannot analyze - over the source byte bound, not
/// UTF-8, or refused by the parser - so its revision contributes no version.
///
/// # Errors
///
/// Returns [`ReadError`] when the object store cannot be read; every
/// per-blob analysis refusal degrades to `None` instead.
fn parse_blob(
    repository: &Repository,
    provider: &dyn SyntaxProvider,
    syntax: SyntaxLimits,
    path: &ProjectPath,
    blob: &TreeFile,
) -> Result<Option<ParsedRevision>, ReadError> {
    let bytes = match repository.blob_bytes(blob, syntax.source_bytes_max()) {
        Ok(bytes) => bytes,
        Err(error) => {
            return match error.fault() {
                HistoryFault::BlobTooLarge { .. } => Ok(None),
                _ => Err(ReadFault::history(error)),
            };
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    let document = provider
        .analyze(SyntaxSource { path, text: &text }, syntax)
        .ok();
    Ok(document.map(|document| ParsedRevision { text, document }))
}

/// One declaration's state at one revision, as the classifier sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SymbolState {
    /// The declaration is in the parsed source, holding these bytes.
    Present(SymbolShape),
    /// The source parsed without the declaration, or the commit removed the
    /// file.
    Absent,
    /// Nothing provable: the blob did not parse, or the revision lies past
    /// the walk's examination bound.
    Unknown,
}

/// The byte regions the classifier compares between two adjacent states of
/// one declaration.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub(crate) struct SymbolShape {
    /// The declaration bytes outside the item node: attached outer
    /// attributes and doc comments.
    attachment: String,
    /// The item node's own bytes.
    item: String,
    /// The body's byte range inside `item`; `None` for a declaration
    /// without one.
    body: Option<std::ops::Range<usize>>,
}

impl SymbolShape {
    /// Cuts one declaration's compared regions out of its file source.
    pub(crate) fn from_source(source: &str, symbol: &SyntaxSymbol) -> Self {
        let mut attachment =
            clipped(source, symbol.range.start, symbol.item_range.start).to_owned();
        attachment.push_str(clipped(source, symbol.item_range.end, symbol.range.end));
        let item = clipped(source, symbol.item_range.start, symbol.item_range.end).to_owned();
        let body = symbol.body_range.map(|body| {
            let start = offset_in(&item, body.start.saturating_sub(symbol.item_range.start));
            let end = offset_in(&item, body.end.saturating_sub(symbol.item_range.start));
            start..end.max(start)
        });
        Self {
            attachment,
            item,
            body,
        }
    }

    /// The item bytes outside the body - what a body edit leaves untouched.
    /// The whole item where no body range is declared.
    fn signature(&self) -> (&str, &str) {
        match &self.body {
            Some(body) => (
                self.item.get(..body.start).unwrap_or(&self.item),
                self.item.get(body.end..).unwrap_or(""),
            ),
            None => (&self.item, ""),
        }
    }
}

/// One provider byte offset clamped into `text`.
fn offset_in(text: &str, offset: u64) -> usize {
    usize::try_from(offset)
        .unwrap_or(text.len())
        .min(text.len())
}

/// `source[start..end]` under clamped bounds; empty for an inverted range.
fn clipped(source: &str, start: u64, end: u64) -> &str {
    let start = offset_in(source, start);
    let end = offset_in(source, end).max(start);
    source.get(start..end).unwrap_or_default()
}

/// Classifies the transition between two adjacent states of one
/// declaration; `None` when the pair proves no change worth a version.
pub(crate) fn classify(older: &SymbolState, newer: &SymbolState) -> Option<SymbolVersionKind> {
    match (older, newer) {
        (SymbolState::Unknown, _)
        | (_, SymbolState::Unknown)
        | (SymbolState::Absent, SymbolState::Absent) => None,
        (SymbolState::Absent, SymbolState::Present(_)) => Some(SymbolVersionKind::Introduced),
        (SymbolState::Present(_), SymbolState::Absent) => Some(SymbolVersionKind::Removed),
        (SymbolState::Present(older), SymbolState::Present(newer)) => item_change(older, newer),
    }
}

/// Which part of a present declaration changed between two revisions.
///
/// The declared interface dominates: bytes outside the body differing is
/// `SignatureChanged` even when the body moved too, and a state without a
/// declared body range on either side treats any item change the same way.
/// Equal items with differing attachment bytes are `DecoratorsChanged`;
/// fully equal states contribute no version.
fn item_change(older: &SymbolShape, newer: &SymbolShape) -> Option<SymbolVersionKind> {
    if older.item != newer.item {
        let bodied = older.body.is_some() && newer.body.is_some();
        let signature_changed = !bodied || older.signature() != newer.signature();
        return Some(if signature_changed {
            SymbolVersionKind::SignatureChanged
        } else {
            SymbolVersionKind::BodyChanged
        });
    }
    (older.attachment != newer.attachment).then_some(SymbolVersionKind::DecoratorsChanged)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rift_core::SourceVisibility;
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::read::{Language, NodeFacet};
    use rift_syntax::{ByteRange, RustSyntaxProvider, SyntaxError};

    use super::*;
    use crate::read::ReadService;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    /// One parsed-state shape cut directly from a source string through the
    /// same extraction the composer runs.
    fn shape(
        source: &str,
        range: ByteRange,
        item: ByteRange,
        body: Option<ByteRange>,
    ) -> SymbolShape {
        let symbol = SyntaxSymbol {
            name: "beacon".to_owned(),
            qualified_name: "beacon".to_owned(),
            container: None,
            kind: "function",
            facets: Vec::new(),
            visibility: None,
            range,
            item_range: item,
            name_range: None,
            body_range: body,
            signatures: Vec::new(),
            documentation: Vec::new(),
            documentation_ranges: Vec::new(),
        };
        SymbolShape::from_source(source, &symbol)
    }

    /// `fn beacon() { <body> }` with an optional attribute prefix, shaped
    /// the way the rust provider ranges it.
    fn function_shape(attachment: &str, signature: &str, body: &str) -> SymbolShape {
        let source = format!("{attachment}{signature}{{{body}}}");
        let item_start = attachment.len() as u64;
        let body_start = (attachment.len() + signature.len() + 1) as u64;
        shape(
            &source,
            ByteRange {
                start: 0,
                end: source.len() as u64,
            },
            ByteRange {
                start: item_start,
                end: source.len() as u64,
            },
            Some(ByteRange {
                start: body_start,
                end: (source.len() - 1) as u64,
            }),
        )
    }

    /// A declaration without a body range, such as `pub struct Beacon;`.
    fn bodyless_shape(item: &str) -> SymbolShape {
        shape(
            item,
            ByteRange {
                start: 0,
                end: item.len() as u64,
            },
            ByteRange {
                start: 0,
                end: item.len() as u64,
            },
            None,
        )
    }

    fn present(shape: SymbolShape) -> SymbolState {
        SymbolState::Present(shape)
    }

    #[test]
    fn classify_absent_to_present_is_introduced() {
        let newer = present(function_shape("", "fn beacon() ", " 1 "));
        assert_eq!(
            classify(&SymbolState::Absent, &newer),
            Some(SymbolVersionKind::Introduced)
        );
    }

    #[test]
    fn classify_present_to_absent_is_removed() {
        let older = present(function_shape("", "fn beacon() ", " 1 "));
        assert_eq!(
            classify(&older, &SymbolState::Absent),
            Some(SymbolVersionKind::Removed)
        );
    }

    #[test]
    fn classify_signature_change_dominates_a_moved_body() {
        let older = present(function_shape("", "fn beacon() ", " 1 "));
        let newer = present(function_shape("", "fn beacon() -> u8 ", " 7 "));
        assert_eq!(
            classify(&older, &newer),
            Some(SymbolVersionKind::SignatureChanged)
        );
    }

    #[test]
    fn classify_body_only_change_is_body_changed() {
        let older = present(function_shape("", "fn beacon() ", " 1 "));
        let newer = present(function_shape("", "fn beacon() ", " 2 "));
        assert_eq!(
            classify(&older, &newer),
            Some(SymbolVersionKind::BodyChanged)
        );
    }

    #[test]
    fn classify_attachment_only_change_is_decorators_changed() {
        let older = present(function_shape("", "fn beacon() ", " 1 "));
        let newer = present(function_shape("#[inline]\n", "fn beacon() ", " 1 "));
        assert_eq!(
            classify(&older, &newer),
            Some(SymbolVersionKind::DecoratorsChanged)
        );
    }

    #[test]
    fn classify_bodyless_item_change_is_signature_changed() {
        let older = present(bodyless_shape("pub struct Beacon;"));
        let newer = present(bodyless_shape("pub struct Beacon(u8);"));
        assert_eq!(
            classify(&older, &newer),
            Some(SymbolVersionKind::SignatureChanged)
        );
    }

    #[test]
    fn classify_unknown_on_either_side_contributes_no_version() {
        let known = present(function_shape("", "fn beacon() ", " 1 "));
        assert_eq!(classify(&SymbolState::Unknown, &known), None);
        assert_eq!(classify(&known, &SymbolState::Unknown), None);
        assert_eq!(classify(&SymbolState::Unknown, &SymbolState::Absent), None);
    }

    #[test]
    fn classify_absent_on_both_sides_contributes_no_version() {
        assert_eq!(classify(&SymbolState::Absent, &SymbolState::Absent), None);
    }

    #[test]
    fn classify_equal_states_contribute_no_version() {
        let older = present(function_shape("#[inline]\n", "fn beacon() ", " 1 "));
        let newer = present(function_shape("#[inline]\n", "fn beacon() ", " 1 "));
        assert_eq!(classify(&older, &newer), None);
    }

    #[test]
    fn shape_splits_attachment_item_and_body() {
        let built = function_shape("#[inline]\n", "fn beacon() ", " 1 ");
        assert_eq!(built.attachment, "#[inline]\n");
        assert_eq!(built.item, "fn beacon() { 1 }");
        assert_eq!(built.signature(), ("fn beacon() {", "}"));
    }

    #[test]
    fn shape_clamps_ranges_past_the_source_end() {
        let clamped = shape(
            "fn f()",
            ByteRange { start: 0, end: 400 },
            ByteRange { start: 2, end: 400 },
            Some(ByteRange {
                start: 300,
                end: 400,
            }),
        );
        assert_eq!(clamped.item, " f()");
        assert_eq!(clamped.signature(), (" f()", ""));
    }

    /// The rust provider behind an analyze call counter, so a test proves
    /// how many parses one composition actually ran.
    #[derive(Debug)]
    struct CountingProvider {
        inner: RustSyntaxProvider,
        analyzed: AtomicUsize,
    }

    impl CountingProvider {
        fn new() -> Self {
            Self {
                inner: RustSyntaxProvider::default(),
                analyzed: AtomicUsize::new(0),
            }
        }

        fn analyzed(&self) -> usize {
            self.analyzed.load(Ordering::SeqCst)
        }
    }

    impl SyntaxProvider for CountingProvider {
        fn language(&self) -> &Language {
            self.inner.language()
        }

        fn analyze(
            &self,
            source: SyntaxSource<'_>,
            limits: SyntaxLimits,
        ) -> Result<SyntaxDocument, SyntaxError> {
            self.analyzed.fetch_add(1, Ordering::SeqCst);
            self.inner.analyze(source, limits)
        }

        fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
            self.inner.node_facets(kind)
        }
    }

    /// A current-tree snapshot of `root` under the default bounds and history table.
    fn current(root: &Path) -> TestResult<ReadService> {
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service = ReadService::build(root, limits, &visibility, &inclusion, history)?;
        Ok(service)
    }

    /// Two declarations sharing one file across two commits, served through
    /// a current-tree read whose files match the second commit.
    fn shared_path_fixture() -> TestResult<(tempfile::TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon_one() {}\npub fn beacon_two() {}\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "introduce both");
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon_one() { let _grown = 1; }\npub fn beacon_two() { let _grown = 2; }\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "grow both");
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
    fn timelines_sharing_one_path_walk_once_and_parse_each_blob_once() -> TestResult {
        let (directory, service) = shared_path_fixture()?;
        let provider = CountingProvider::new();
        let mut timelines = SymbolTimelines::open(
            directory.path(),
            None,
            &HistoryConfiguration::default(),
            SyntaxLimits::default(),
            None,
        )
        .map_err(|error| error.to_string())?;
        for name in ["beacon_one", "beacon_two"] {
            let matches = service
                .index()
                .symbols(name, 5)
                .map_err(|error| error.to_string())?;
            let timeline = timelines
                .timeline(&provider, matches[0])
                .map_err(|error| error.to_string())?;
            assert_eq!(
                timeline.versions.len(),
                2,
                "{name} grew after its introduction"
            );
        }
        let TimelineSource::Walk(walked) = &timelines.source else {
            panic!("no store is attached, so the timelines walk git");
        };
        assert_eq!(walked.walks.len(), 1, "two hits on one path share one walk");
        assert_eq!(
            provider.analyzed(),
            2,
            "two commits hold two distinct blobs; four states reuse them"
        );
        Ok(())
    }

    #[test]
    fn open_refuses_a_disabled_history_provider() {
        let directory = tempfile::tempdir().expect("temp dir");
        let disabled = HistoryConfiguration {
            enabled: false,
            ..HistoryConfiguration::default()
        };
        let error = SymbolTimelines::open(
            directory.path(),
            None,
            &disabled,
            SyntaxLimits::default(),
            None,
        )
        .expect_err("a disabled provider must refuse before any repository access");
        assert!(matches!(error.fault(), ReadFault::Unsupported { .. }));
    }

    #[test]
    fn open_refuses_an_unborn_head() {
        let directory = tempfile::tempdir().expect("temp dir");
        rift_history::fixture::init(directory.path());
        let error = SymbolTimelines::open(
            directory.path(),
            None,
            &HistoryConfiguration::default(),
            SyntaxLimits::default(),
            None,
        )
        .expect_err("a repository without commits resolves no HEAD");
        assert!(matches!(error.fault(), ReadFault::History(_)));
    }

    /// One `beacon_one` timeline over the shared-path fixture, composed
    /// under `history`.
    fn beacon_timeline(
        root: &Path,
        service: &ReadService,
        history: &HistoryConfiguration,
    ) -> TestResult<SymbolHistory> {
        let mut timelines =
            SymbolTimelines::open(root, None, history, SyntaxLimits::default(), None)
                .map_err(|error| error.to_string())?;
        let matches = service
            .index()
            .symbols("beacon_one", 5)
            .map_err(|error| error.to_string())?;
        let timeline = timelines
            .timeline(&RustSyntaxProvider::default(), matches[0])
            .map_err(|error| error.to_string())?;
        Ok(timeline)
    }

    #[test]
    fn timeline_over_a_whole_history_is_complete() -> TestResult {
        let (directory, service) = shared_path_fixture()?;
        let timeline =
            beacon_timeline(directory.path(), &service, &HistoryConfiguration::default())?;
        assert!(
            timeline.complete,
            "a walk that reached the path's first commit covered its whole history"
        );
        Ok(())
    }

    #[test]
    fn timeline_cut_by_the_revision_bound_is_incomplete() -> TestResult {
        let (directory, service) = shared_path_fixture()?;
        let bounded = HistoryConfiguration {
            max_revisions: 1,
            ..HistoryConfiguration::default()
        };
        let timeline = beacon_timeline(directory.path(), &service, &bounded)?;
        assert!(
            !timeline.complete,
            "a walk the max_revisions bound stopped never reached the first commit"
        );
        Ok(())
    }

    #[test]
    fn timeline_at_a_shallow_boundary_is_incomplete() -> TestResult {
        let (directory, service) = shared_path_fixture()?;
        let head = Repository::open(directory.path())
            .map_err(|error| error.to_string())?
            .resolve("HEAD")
            .map_err(|error| error.to_string())?;
        fs::write(
            directory.path().join(".git/shallow"),
            format!("{}\n", head.commit_id()),
        )?;
        let timeline =
            beacon_timeline(directory.path(), &service, &HistoryConfiguration::default())?;
        assert!(
            !timeline.complete,
            "a clone whose shallow file names the served commit holds none of its parents"
        );
        assert!(
            timeline.versions.is_empty(),
            "nothing older than the boundary is provable, so no version is classified"
        );
        Ok(())
    }

    /// A history store at `store_folder`, filled with every commit `history`
    /// selects in the workspace at `root`.
    fn filled_store(
        root: &Path,
        store_folder: &Path,
        history: &HistoryConfiguration,
    ) -> TestResult<rift_history_store::HistoryStore> {
        let location = rift_history_store::StoreLocation::new(store_folder, "aa");
        let store = rift_history_store::HistoryStore::open(&location)?;
        let mut filler = store.filler()?.ok_or("no other filler runs")?;
        let analysis = crate::HistoryAnalysis::open(
            root,
            history,
            (
                &SourceVisibility::default(),
                &rift_core::TextFileInclusion::default(),
                &rift_core::LanguageFileSelections::default(),
            ),
            SyntaxLimits::default(),
        )
        .map_err(|error| error.to_string())?;
        let held = filler.held()?;
        let plan = analysis.plan(&held).map_err(|error| error.to_string())?;
        let mut records = Vec::new();
        for pending in plan.pending() {
            let analyzed = analysis
                .analyze(pending, &|| false)
                .map_err(|error| error.to_string())?
                .ok_or("nothing cancels the analysis")?;
            records.push(analyzed.into_record());
        }
        filler.write_batch(&records)?;
        filler.trim(plan.keep())?;
        Ok(store)
    }

    /// The store handle a read attaches, counting how often a read found the
    /// store lagging.
    fn stored(
        store: &rift_history_store::HistoryStore,
        strategy: HistoryStrategy,
    ) -> (StoredHistory, Arc<AtomicUsize>) {
        let lagged = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&lagged);
        let stored = StoredHistory::new(
            store.reader(),
            strategy,
            Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );
        (stored, lagged)
    }

    /// One timeline of `name` from `service`'s index, composed from `stored`.
    fn stored_timeline(
        root: &Path,
        service: &ReadService,
        history: &HistoryConfiguration,
        stored: &StoredHistory,
        name: &str,
    ) -> TestResult<SymbolHistory> {
        let mut timelines =
            SymbolTimelines::open(root, None, history, SyntaxLimits::default(), Some(stored))
                .map_err(|error| error.to_string())?;
        let matches = service
            .index()
            .symbols(name, 5)
            .map_err(|error| error.to_string())?;
        let timeline = timelines
            .timeline(&RustSyntaxProvider::default(), matches[0])
            .map_err(|error| error.to_string())?;
        Ok(timeline)
    }

    #[test]
    fn a_stored_timeline_answers_what_a_walk_answers() -> TestResult {
        let (directory, service) = shared_path_fixture()?;
        let folder = tempfile::tempdir()?;
        let history = HistoryConfiguration::default();
        let store = filled_store(directory.path(), folder.path(), &history)?;
        let (stored, lagged) = stored(&store, HistoryStrategy::Everything);

        let from_store =
            stored_timeline(directory.path(), &service, &history, &stored, "beacon_one")?;
        let walked = beacon_timeline(directory.path(), &service, &history)?;

        assert_eq!(from_store, walked);
        assert!(from_store.complete);
        assert_eq!(from_store.versions.len(), 2);
        assert_eq!(from_store.versions[0].author.name, "Rift Fixture");
        assert_eq!(lagged.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn a_stored_timeline_lags_until_the_store_holds_the_served_commit() -> TestResult {
        let (directory, service) = shared_path_fixture()?;
        let folder = tempfile::tempdir()?;
        let history = HistoryConfiguration::default();
        let location = rift_history_store::StoreLocation::new(folder.path(), "aa");
        let empty = rift_history_store::HistoryStore::open(&location)?;
        let (stored_empty, lagged) = stored(&empty, HistoryStrategy::Everything);
        let root = directory.path();

        let lagging = stored_timeline(root, &service, &history, &stored_empty, "beacon_one")?;

        assert!(!lagging.complete, "the store holds nothing yet");
        assert!(lagging.versions.is_empty());
        assert_eq!(lagged.load(Ordering::SeqCst), 1, "the read asks for a fill");
        drop(stored_empty);
        drop(empty);

        let store = filled_store(directory.path(), folder.path(), &history)?;
        let (stored_filled, lagged) = stored(&store, HistoryStrategy::Everything);
        let caught_up = stored_timeline(root, &service, &history, &stored_filled, "beacon_one")?;
        assert!(caught_up.complete, "the store holds the whole history now");
        assert_eq!(lagged.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn a_stored_timeline_past_the_window_is_incomplete() -> TestResult {
        let (directory, service) = shared_path_fixture()?;
        let folder = tempfile::tempdir()?;
        let history = HistoryConfiguration {
            max_revisions: 1,
            ..HistoryConfiguration::default()
        };
        let store = filled_store(directory.path(), folder.path(), &history)?;
        let (stored, _) = stored(&store, HistoryStrategy::Everything);

        let timeline =
            stored_timeline(directory.path(), &service, &history, &stored, "beacon_one")?;

        assert!(!timeline.complete);
        let kinds: Vec<SymbolVersionKind> = timeline
            .versions
            .iter()
            .map(|version| version.kind)
            .collect();
        assert_eq!(kinds, [SymbolVersionKind::BodyChanged]);
        Ok(())
    }

    #[test]
    fn a_stored_timeline_follows_a_file_moved_without_a_byte_changed() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        rift_history::fixture::init(root);
        fs::write(root.join("before.rs"), "pub fn travelled() {}\n")?;
        rift_history::fixture::commit_all(root, "introduce travelled");
        let grown = "pub fn travelled() { let _grown = 1; }\n";
        fs::write(root.join("before.rs"), grown)?;
        rift_history::fixture::commit_all(root, "grow travelled");
        rift_history::fixture::git(root, &["mv", "before.rs", "after.rs"]);
        rift_history::fixture::commit_all(root, "move travelled");
        let service = current(root)?;
        let folder = tempfile::tempdir()?;
        let history = HistoryConfiguration::default();
        let store = filled_store(root, folder.path(), &history)?;
        let (stored, _) = stored(&store, HistoryStrategy::Everything);

        let timeline = stored_timeline(root, &service, &history, &stored, "travelled")?;

        let versions: Vec<(SymbolVersionKind, &str)> = timeline
            .versions
            .iter()
            .map(|version| (version.kind, version.path.0.as_str()))
            .collect();
        assert_eq!(
            versions,
            [
                (SymbolVersionKind::Moved, "after.rs"),
                (SymbolVersionKind::BodyChanged, "before.rs"),
                (SymbolVersionKind::Introduced, "before.rs"),
            ]
        );
        assert!(timeline.complete);
        Ok(())
    }

    #[test]
    fn a_selective_timeline_lists_the_releases_that_changed_the_declaration() -> TestResult {
        let (directory, service) = shared_path_fixture()?;
        let root = directory.path();
        rift_history::fixture::git(root, &["tag", "v1.0.0", "HEAD~1"]);
        rift_history::fixture::git(root, &["tag", "v2.0.0", "HEAD"]);
        let folder = tempfile::tempdir()?;
        let history = HistoryConfiguration {
            strategy: HistoryStrategy::Selective,
            releases: vec!["v*".to_owned()],
            ..HistoryConfiguration::default()
        };
        let store = filled_store(root, folder.path(), &history)?;
        let (stored, lagged) = stored(&store, HistoryStrategy::Selective);

        let timeline = stored_timeline(root, &service, &history, &stored, "beacon_one")?;

        let kinds: Vec<SymbolVersionKind> = timeline
            .versions
            .iter()
            .map(|version| version.kind)
            .collect();
        assert_eq!(kinds, [SymbolVersionKind::BodyChanged]);
        assert!(
            !timeline.complete,
            "the oldest selected release is compared with nothing"
        );
        assert_eq!(lagged.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn a_stored_timeline_follows_a_declaration_moved_into_another_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        rift_history::fixture::init(root);
        let introduced = "pub fn travelled() {\n    let x = 1;\n}\npub fn stays() {}\n";
        fs::write(root.join("from.rs"), introduced)?;
        rift_history::fixture::commit_all(root, "introduce travelled");
        fs::remove_file(root.join("from.rs"))?;
        let moved = "pub fn travelled() {\n    let x = 1;\n}\npub fn arrived() {}\n";
        fs::write(root.join("to.rs"), moved)?;
        rift_history::fixture::commit_all(root, "move travelled");
        let service = current(root)?;
        let folder = tempfile::tempdir()?;
        let history = HistoryConfiguration::default();
        let store = filled_store(root, folder.path(), &history)?;
        let (stored, _) = stored(&store, HistoryStrategy::Everything);

        let timeline = stored_timeline(root, &service, &history, &stored, "travelled")?;

        let versions: Vec<(SymbolVersionKind, &str)> = timeline
            .versions
            .iter()
            .map(|version| (version.kind, version.path.0.as_str()))
            .collect();
        assert_eq!(
            versions,
            [
                (SymbolVersionKind::Moved, "to.rs"),
                (SymbolVersionKind::Introduced, "from.rs"),
            ]
        );
        assert!(timeline.complete);
        Ok(())
    }

    #[test]
    fn a_moved_head_owes_only_its_new_commits_and_a_rewrite_trims_the_orphans() -> TestResult {
        let (directory, _service) = shared_path_fixture()?;
        let root = directory.path();
        let folder = tempfile::tempdir()?;
        let history = HistoryConfiguration::default();
        let store = filled_store(root, folder.path(), &history)?;
        let analysis = crate::HistoryAnalysis::open(
            root,
            &history,
            (
                &SourceVisibility::default(),
                &rift_core::TextFileInclusion::default(),
                &rift_core::LanguageFileSelections::default(),
            ),
            SyntaxLimits::default(),
        )
        .map_err(|error| error.to_string())?;
        let mut filler = store.filler()?.ok_or("no other filler runs")?;
        let orphan = Repository::open(root)
            .map_err(|error| error.to_string())?
            .resolve("HEAD")
            .map_err(|error| error.to_string())?
            .commit_id();

        fs::write(root.join("lib.rs"), "pub fn beacon_one() {}\n")?;
        rift_history::fixture::commit_all(root, "shrink beacon_one");
        let moved = analysis
            .plan(&filler.held()?)
            .map_err(|error| error.to_string())?;
        assert_eq!(moved.pending().len(), 1, "only the new commit is owed");

        rift_history::fixture::git(root, &["reset", "-q", "--hard", "HEAD~2"]);
        let rewritten_source = "pub fn beacon_one() { let _rewritten = 1; }\n";
        fs::write(root.join("lib.rs"), rewritten_source)?;
        rift_history::fixture::commit_all(root, "rewrite");
        let rewritten = analysis
            .plan(&filler.held()?)
            .map_err(|error| error.to_string())?;
        assert_eq!(
            filler.trim(rewritten.keep())?,
            1,
            "the rewritten-away commit leaves"
        );
        let held = filler.held()?;
        assert!(!held.contains_key(&orphan));
        assert_eq!(
            held.len(),
            1,
            "the root commit every window still reaches stays"
        );
        Ok(())
    }
}
