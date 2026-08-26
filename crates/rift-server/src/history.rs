//! Symbol timelines: version-control history read through the syntax tier.
//!
//! One request's composition walks each hit path's first-parent history,
//! parses the committed blobs with the path's syntax provider, and
//! classifies each adjacent pair of parsed states into a wire
//! [`SymbolVersionKind`]. The caller runs the composition on its blocking
//! lane; the classifier itself is sans-I/O.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;

use rift_core::ProjectPath;
use rift_history::{
    HistoryFault, PathHistory, PathRevision, Repository, ResolvedRevision, TreeFile,
};
use rift_index::SymbolMatch;
use rift_protocol::configuration::{HISTORY_REVISIONS_MAX, HistoryConfiguration};
use rift_protocol::read::{RevisionId, SymbolHistory, SymbolVersion, SymbolVersionKind};
use rift_syntax::{SyntaxDocument, SyntaxProvider, SyntaxSource, SyntaxSymbol};

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

/// Per-request timeline composition over one served revision: one
/// repository handle, one walk per distinct hit path, one parse per
/// distinct committed blob.
#[derive(Debug)]
pub(crate) struct SymbolTimelines {
    repository: Repository,
    start: ResolvedRevision,
    revisions_max: usize,
    walks: HashMap<String, PathHistory>,
    parses: HashMap<ParseKey, Option<ParsedRevision>>,
}

impl SymbolTimelines {
    /// Opens the workspace repository and resolves where timelines start:
    /// the served commit for a revision read, `HEAD` for a current-tree
    /// read. The walk budget is the configured history depth under the
    /// protocol's hard bound.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`]: `unsupported` when `[providers.history]` is
    /// disabled, and the version-control fault when the workspace has no
    /// repository or its head does not resolve.
    pub(crate) fn open(
        root: &Path,
        revision: Option<&RevisionId>,
        history: &HistoryConfiguration,
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
        Ok(Self {
            repository,
            start,
            revisions_max,
            walks: HashMap::new(),
            parses: HashMap::new(),
        })
    }

    /// Composes one hit's timeline: the path's touching commits newest
    /// first, each parsed through `provider` and classified against its
    /// adjacent older state.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the repository cannot be read. A blob the
    /// tier cannot analyze contributes no version instead of failing.
    pub(crate) fn timeline(
        &mut self,
        provider: &dyn SyntaxProvider,
        matched: SymbolMatch<'_>,
    ) -> Result<SymbolHistory, ReadError> {
        let path = matched.file.path();
        let Self {
            repository,
            start,
            revisions_max,
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
                path,
                revision,
                &matched.symbol.qualified_name,
            )?);
        }
        // The state past the oldest walked commit: provably absent when the
        // walk covered the path's whole history, unknown - contributing no
        // version - when the examination bound cut the walk short.
        let boundary = if history.is_complete() {
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
            });
        }
        Ok(SymbolHistory {
            symbol: symbol_id(matched.file, matched.symbol),
            versions,
        })
    }
}

/// The declaration's state at one touching commit: parsed from the
/// committed blob, absent when the commit removed the file or the parse
/// lacks the symbol, unknown when the blob cannot be analyzed.
fn revision_state(
    repository: &Repository,
    parses: &mut HashMap<ParseKey, Option<ParsedRevision>>,
    provider: &dyn SyntaxProvider,
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
        Entry::Vacant(vacant) => vacant.insert(parse_blob(repository, provider, path, blob)?),
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

/// Parses one committed blob through the provider. `None` marks a blob the
/// tier cannot analyze - over the provider's byte bound, not UTF-8, or
/// refused by the parser - so its revision contributes no version.
///
/// # Errors
///
/// Returns [`ReadError`] when the object store cannot be read; every
/// per-blob analysis refusal degrades to `None` instead.
fn parse_blob(
    repository: &Repository,
    provider: &dyn SyntaxProvider,
    path: &ProjectPath,
    blob: &TreeFile,
) -> Result<Option<ParsedRevision>, ReadError> {
    let bytes = match repository.blob_bytes(blob, provider.source_bytes_max()) {
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
    let document = provider.analyze(SyntaxSource { path, text: &text }).ok();
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    fn from_source(source: &str, symbol: &SyntaxSymbol) -> Self {
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
            body_range: body,
            signatures: Vec::new(),
            documentation: Vec::new(),
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

        fn extensions(&self) -> &'static [&'static str] {
            self.inner.extensions()
        }

        fn source_bytes_max(&self) -> usize {
            self.inner.source_bytes_max()
        }

        fn analyze(&self, source: SyntaxSource<'_>) -> Result<SyntaxDocument, SyntaxError> {
            self.analyzed.fetch_add(1, Ordering::SeqCst);
            self.inner.analyze(source)
        }

        fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
            self.inner.node_facets(kind)
        }
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
        let mut timelines =
            SymbolTimelines::open(directory.path(), None, &HistoryConfiguration::default())
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
        assert_eq!(
            timelines.walks.len(),
            1,
            "two hits on one path share one walk"
        );
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
            max_revisions: 500,
        };
        let error = SymbolTimelines::open(directory.path(), None, &disabled)
            .expect_err("a disabled provider must refuse before any repository access");
        assert!(matches!(error.fault(), ReadFault::Unsupported { .. }));
    }

    #[test]
    fn open_refuses_an_unborn_head() {
        let directory = tempfile::tempdir().expect("temp dir");
        rift_history::fixture::init(directory.path());
        let error = SymbolTimelines::open(directory.path(), None, &HistoryConfiguration::default())
            .expect_err("a repository without commits resolves no HEAD");
        assert!(matches!(error.fault(), ReadFault::History(_)));
    }
}
