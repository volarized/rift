//! Incoming reference traversal through configured language engines.

use std::collections::{BTreeMap, BTreeSet};

use lsp_types::{Location, Position};
use rift_core::{ProjectPath, SymbolId as CoreSymbolId};
use rift_index::{IndexedFile, RelationshipStore};
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::position::LineIndex;
use rift_lsp::session::{EngineError, EngineFault, EngineSession};
use rift_lsp::uri::{TreeRoot, UriFault};
use rift_protocol::read::{
    ExactKind, Extensions, GraphHop, HopDirection, Relationship, RelationshipDerivation,
    RelationshipFacet, SearchParams, SearchParamsTarget, SearchTraversal, SymbolId,
    TraversalDirection,
};
use rift_syntax::SyntaxSymbol;

use crate::engine::{EnginePool, EngineSlot};
use crate::read::{ReadError, ReadFault, ReadService, symbol_id};
use crate::traversal::{
    TRAVERSAL_NODES_MAX, resolve_graph_symbol, validate_traversal, walk_traversal_with_references,
};

/// References resolved from one published source revision.
#[derive(Debug, Default)]
pub struct EngineReferences {
    revision: Option<String>,
    incoming: BTreeMap<CoreSymbolId, Vec<GraphHop>>,
}

impl EngineReferences {
    /// Whether the engines contributed any traversal edges.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.incoming.values().all(Vec::is_empty)
    }

    #[cfg(test)]
    pub(crate) fn from_incoming(incoming: BTreeMap<CoreSymbolId, Vec<GraphHop>>) -> Self {
        Self {
            incoming,
            ..Self::default()
        }
    }

    pub(crate) fn resolved(&self, symbol: &SymbolId) -> bool {
        self.incoming
            .keys()
            .any(|identity| identity.as_str() == symbol.0)
    }

    pub(crate) fn incoming(&self, symbol: &CoreSymbolId) -> &[GraphHop] {
        self.incoming.get(symbol).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn validate_revision(&self, reads: &ReadService) -> Result<(), ReadError> {
        if self
            .revision
            .as_deref()
            .is_some_and(|revision| revision != reads.tree_revision())
        {
            return Err(ReadFault::unavailable(
                "engine references",
                "source revision changed before search",
            ));
        }
        Ok(())
    }
}

/// Whether this search can request references from a configured language engine.
///
/// The indexed walk includes intermediate declarations that a depth-two traversal can
/// reach before its first engine request. No engine starts during this check.
///
/// # Errors
///
/// Returns the same request validation errors as the reference resolver.
pub fn uses_engine_references(
    reads: &ReadService,
    engines: &EnginePool,
    params: &SearchParams,
) -> Result<bool, ReadError> {
    reads.validate_engine_search(params)?;
    let Some(traversal) = reference_traversal(params) else {
        return Ok(false);
    };
    validate_traversal(traversal)?;
    let seed = CoreSymbolId::new(traversal.seed.0.clone())
        .map_err(|_| ReadFault::invalid("traversal.seed", "not a symbol identity"))?;
    Ok(reachable_reference_source(
        reads.relationships(),
        &seed,
        traversal,
        |identity| reference_source(reads, engines, identity).is_some(),
    ))
}

fn reachable_reference_source(
    store: &RelationshipStore,
    seed: &CoreSymbolId,
    traversal: &SearchTraversal,
    has_source: impl Fn(&CoreSymbolId) -> bool,
) -> bool {
    if has_source(seed) {
        return true;
    }
    if traversal.depth == 1 {
        return false;
    }
    let mut prior = traversal.clone();
    prior.depth -= 1;
    walk_traversal_with_references(
        store,
        seed,
        &prior,
        TRAVERSAL_NODES_MAX,
        &EngineReferences::default(),
    )
    .discovered
    .into_iter()
    .any(|(identity, _)| has_source(&identity))
}

fn reference_traversal(params: &SearchParams) -> Option<&SearchTraversal> {
    params.traversal.as_ref().filter(|traversal| {
        let current = params.rev.is_none();
        let symbols = matches!(
            params.target,
            SearchParamsTarget::All | SearchParamsTarget::Symbol
        );
        let incoming = matches!(
            traversal.direction,
            TraversalDirection::Incoming | TraversalDirection::Both
        );
        let references = traversal.facets.is_empty()
            || traversal.facets.contains(&RelationshipFacet::References);
        current && symbols && incoming && references
    })
}

fn reference_source<'source>(
    reads: &'source ReadService,
    engines: &'source EnginePool,
    identity: &CoreSymbolId,
) -> Option<(
    &'source EngineSlot,
    &'source IndexedFile,
    &'source SyntaxSymbol,
)> {
    let (file, symbol) = resolve_graph_symbol(reads.index(), identity)?;
    symbol.name_range?;
    let slot = engines.engine_for(file.syntax().language())?;
    Some((slot, file, symbol))
}

/// Resolves incoming references while retaining every indexed relationship.
///
/// Only current-tree symbol traversals with the `references` facet consult engines.
/// The traversal's existing depth and node bounds also bound engine requests. Each
/// exchange uses the selected engine's retry and restart policy. The caller applies
/// its request deadline and validates the captured tree again before using the result.
/// Engines without references support leave the indexed answer unchanged.
///
/// # Errors
///
/// Returns invalid traversal, engine failure, or an engine range outside published source.
///
/// # Cancel safety
///
/// Dropping the future discards the selected session and its open documents.
/// No source file is written.
pub async fn resolve_engine_references(
    reads: &ReadService,
    engines: &EnginePool,
    params: &SearchParams,
) -> Result<EngineReferences, ReadError> {
    if !uses_engine_references(reads, engines, params)? {
        return Ok(EngineReferences::default());
    }
    let Some(traversal) = reference_traversal(params) else {
        return Ok(EngineReferences::default());
    };
    validate_traversal(traversal)?;
    let seed = CoreSymbolId::new(traversal.seed.0.clone())
        .map_err(|_| ReadFault::invalid("traversal.seed", "not a symbol identity"))?;
    let mut references = EngineReferences {
        revision: Some(reads.tree_revision().to_owned()),
        ..EngineReferences::default()
    };
    let mut pending = vec![seed.clone()];
    let mut requested = BTreeSet::new();
    for depth in 0..traversal.depth {
        extend_references(reads, engines, &mut references, pending, &mut requested).await?;
        if depth + 1 == traversal.depth {
            break;
        }
        let mut step = traversal.clone();
        step.depth = depth + 1;
        pending = walk_traversal_with_references(
            reads.relationships(),
            &seed,
            &step,
            TRAVERSAL_NODES_MAX,
            &references,
        )
        .discovered
        .into_iter()
        .map(|(symbol, _)| symbol)
        .collect();
    }
    Ok(references)
}

async fn extend_references(
    reads: &ReadService,
    engines: &EnginePool,
    references: &mut EngineReferences,
    pending: Vec<CoreSymbolId>,
    requested: &mut BTreeSet<CoreSymbolId>,
) -> Result<(), ReadError> {
    let stored: usize = references.incoming.values().map(Vec::len).sum();
    let mut remaining = TRAVERSAL_NODES_MAX - stored;
    for identity in pending {
        if !requested.insert(identity.clone()) {
            continue;
        }
        let Some(edges) = resolve_symbol_references(reads, engines, &identity).await? else {
            continue;
        };
        remaining = remaining.checked_sub(edges.len()).ok_or_else(|| {
            ReadFault::unavailable(
                "engine references",
                "reference edges exceed the traversal node bound",
            )
        })?;
        references.incoming.insert(identity, edges);
    }
    Ok(())
}

async fn resolve_symbol_references(
    reads: &ReadService,
    engines: &EnginePool,
    identity: &CoreSymbolId,
) -> Result<Option<Vec<GraphHop>>, ReadError> {
    let Some((slot, file, symbol)) = reference_source(reads, engines, identity) else {
        return Ok(None);
    };
    let target = ReferenceTarget::new(slot.workspace_root(), reads.index().root(), file, symbol)?;
    let report = match references_on_engine(slot, &target).await {
        Ok(report) => report,
        Err(error) if matches!(error.fault(), EngineFault::CapabilityAbsent { .. }) => {
            return Ok(None);
        }
        Err(error) => return Err(ReadFault::engine(error)),
    };
    map_references(reads, identity, report, slot.workspace_root()).map(Some)
}

struct ReferenceTarget {
    path: ProjectPath,
    language: String,
    source: String,
    utf8: Position,
    utf16: Position,
    uri: lsp_types::Uri,
    canonical_uri: lsp_types::Uri,
}

impl ReferenceTarget {
    fn new(
        workspace_root: &std::path::Path,
        canonical_root: &std::path::Path,
        file: &IndexedFile,
        symbol: &SyntaxSymbol,
    ) -> Result<Self, ReadError> {
        let source = file.source();
        let offset = declaration_name_offset(file, symbol)?;
        let index = LineIndex::new(source);
        let position = |encoding| {
            index
                .position(encoding, offset)
                .map_err(|error| ReadFault::task("reference position conversion", error.detail()))
        };
        Ok(Self {
            path: file.path().clone(),
            language: file.syntax().language().name.clone(),
            uri: TreeRoot::new(workspace_root)
                .and_then(|root| root.document_uri(file.path()))
                .map_err(|error| ReadFault::task("reference URI conversion", error.detail()))?,
            canonical_uri: TreeRoot::new(canonical_root)
                .and_then(|root| root.document_uri(file.path()))
                .map_err(|error| ReadFault::task("reference URI conversion", error.detail()))?,
            source: source.to_owned(),
            utf8: position(PositionEncoding::Utf8)?,
            utf16: position(PositionEncoding::Utf16)?,
        })
    }
}

#[derive(PartialEq)]
struct ReferenceReport {
    locations: Result<Vec<Location>, usize>,
    encoding: PositionEncoding,
    full: bool,
}

async fn references_on_engine(
    slot: &EngineSlot,
    target: &ReferenceTarget,
) -> Result<ReferenceReport, EngineError> {
    let open_path = target.path.clone();
    let open_language = target.language.clone();
    let open_source = target.source.clone();
    let request_path = target.path.clone();
    let close_path = target.path.clone();
    let utf8 = target.utf8;
    let utf16 = target.utf16;
    let uri = target.uri.clone();
    let canonical_uri = target.canonical_uri.clone();
    slot.request_settled(
        move |session: &mut EngineSession| {
            let path = open_path.clone();
            let language = open_language.clone();
            let source = open_source.clone();
            Box::pin(async move { session.open(&path, &language, source).await })
        },
        move |session: &mut EngineSession| {
            let path = request_path.clone();
            let uri = uri.clone();
            let canonical_uri = canonical_uri.clone();
            Box::pin(async move {
                let encoding = session.capabilities().position_encoding;
                let position = match encoding {
                    PositionEncoding::Utf8 => utf8,
                    PositionEncoding::Utf16 => utf16,
                };
                let mut locations = session.references(&path, position).await?;
                if locations.len() > TRAVERSAL_NODES_MAX {
                    return Ok(ReferenceReport { locations: Err(locations.len()), encoding, full: true });
                }
                locations.sort_by(|left, right| {
                    (
                        left.uri.as_str(),
                        left.range.start.line,
                        left.range.start.character,
                        left.range.end.line,
                        left.range.end.character,
                    )
                        .cmp(&(
                            right.uri.as_str(),
                            right.range.start.line,
                            right.range.start.character,
                            right.range.end.line,
                            right.range.end.character,
                        ))
                });
                locations.dedup();
                let full = locations.iter().any(|location| {
                    (location.uri == uri || location.uri == canonical_uri)
                        && location.range.start <= position
                        && position < location.range.end
                });
                Ok(ReferenceReport {
                    locations: Ok(locations),
                    encoding,
                    full,
                })
            })
        },
        move |session: &mut EngineSession| {
            let path = close_path.clone();
            Box::pin(async move {
                // The references were answered already; a failed closing notification
                // cannot change those facts. The failure remains recorded in engine logs.
                if let Err(error) = session.close(&path).await {
                    tracing::warn!(component = "engine", operation = "textDocument/didClose", %error, "engine document close failed");
                }
            })
        },
        |report| (report.full, report.locations.as_ref().is_ok_and(|locations| locations.len() <= 1)),
        |_report| None,
    )
    .await
}

fn map_references(
    reads: &ReadService,
    target: &CoreSymbolId,
    report: ReferenceReport,
    workspace_root: &std::path::Path,
) -> Result<Vec<GraphHop>, ReadError> {
    let locations = report.locations.map_err(|observed| ReadFault::unavailable(
        "engine references", format!("reference locations {observed} exceed the {TRAVERSAL_NODES_MAX}-node traversal bound")))?;
    let root = TreeRoot::new(workspace_root)
        .map_err(|error| ReadFault::task("reference root conversion", error.detail()))?;
    let mut callers = BTreeSet::new();
    for location in locations {
        if let Some(caller) = reference_caller(reads, &root, &location, report.encoding)?
            && caller.0 != target.as_str()
        {
            callers.insert(caller);
        }
    }
    Ok(callers
        .into_iter()
        .map(|caller| GraphHop {
            relationship: Relationship {
                from: caller,
                to: SymbolId(target.as_str().to_owned()),
                kind: ExactKind(rift_core::fault_label(&RelationshipFacet::References)),
                facets: vec![RelationshipFacet::References],
                evidence: Vec::new(),
                derivation: RelationshipDerivation::Resolution,
                confidence: None,
                extensions: Extensions::default(),
            },
            direction: HopDirection::Incoming,
        })
        .collect())
}

fn reference_caller(
    reads: &ReadService,
    root: &TreeRoot,
    location: &Location,
    encoding: PositionEncoding,
) -> Result<Option<SymbolId>, ReadError> {
    let relative = root.project_path(&location.uri).or_else(|error| {
        if matches!(error.fault(), UriFault::OutsideRoot) {
            TreeRoot::new(reads.index().root())?.project_path(&location.uri)
        } else {
            Err(error)
        }
    });
    let path = match relative {
        Ok(path) => path,
        Err(error) if matches!(error.fault(), UriFault::OutsideRoot) => return Ok(None),
        Err(error) => return Err(ReadFault::task("reference URI conversion", error.detail())),
    };
    let Some(file) = reads.index().file(&path) else {
        return Ok(None);
    };
    let index = LineIndex::new(file.source());
    let convert = |position| {
        index
            .byte_offset(encoding, position)
            .map_err(|error| ReadFault::unavailable("engine references", error.detail()))
    };
    let start = convert(location.range.start)? as u64;
    let end = convert(location.range.end)? as u64;
    if start > end {
        return Err(ReadFault::unavailable(
            "engine references",
            "reference range ends before it starts",
        ));
    }
    Ok(file
        .syntax()
        .symbols()
        .iter()
        .filter(|symbol| symbol.item_range.start <= start && end <= symbol.item_range.end)
        .min_by_key(|symbol| {
            (
                symbol.item_range.end - symbol.item_range.start,
                symbol.item_range.start,
            )
        })
        .map(|symbol| symbol_id(file, symbol)))
}

/// Reads the exact name range retained by the syntax provider.
fn declaration_name_offset(file: &IndexedFile, symbol: &SyntaxSymbol) -> Result<usize, ReadError> {
    let range = symbol
        .name_range
        .ok_or_else(|| ReadFault::unsupported("declaration name range"))?;
    let invalid = || {
        ReadFault::task(
            "reference position conversion",
            "declaration name range exceeds indexed source",
        )
    };
    let start = usize::try_from(range.start).map_err(|_| invalid())?;
    let end = usize::try_from(range.end).map_err(|_| invalid())?;
    if file.source().get(start..end).is_none() {
        return Err(invalid());
    }
    Ok(start)
}

#[cfg(all(test, unix))]
#[path = "../tests/process_lifecycle.rs"]
#[expect(dead_code, reason = "Each suite selects only its process fixtures.")]
mod process_lifecycle;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use lsp_types::{Location, Position, Range};
    use rift_core::{ProjectPath, SourceVisibility, TextFileInclusion};
    use rift_index::WorkspaceIndexLimits;
    use rift_lsp::capabilities::PositionEncoding;
    use rift_lsp::uri::TreeRoot;
    use rift_protocol::configuration::{HistoryConfiguration, LspConfiguration};
    use rift_protocol::read::{GetSymbolParams, SearchParams, SymbolId};
    use serde_json::json;

    use super::{
        EngineReferences, ReferenceReport, declaration_name_offset, map_references,
        resolve_engine_references,
    };
    use crate::{EnginePool, LspProcessKey, ReadService};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn reads(root: &std::path::Path) -> Result<ReadService, crate::ReadError> {
        ReadService::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
    }

    fn symbol(reads: &ReadService, name: &str) -> SymbolId {
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": name})).expect("lookup");
        reads.get_symbol(&params).expect("lookup answer").hits[0]
            .symbol
            .id
            .clone()
            .expect("symbol identity")
    }

    fn request(seed: &SymbolId) -> SearchParams {
        serde_json::from_value(json!({"target":"symbol","traversal":{"seed":seed,"direction":"incoming","facets":["references"]}})).expect("traversal request")
    }

    fn location(root: &std::path::Path, path: &str, line: u32, start: u32, end: u32) -> Location {
        let tree = TreeRoot::new(root).expect("tree root");
        let path = ProjectPath::new(path).expect("project path");
        Location {
            uri: tree.document_uri(&path).expect("document URI"),
            range: Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
        }
    }

    fn pool(root: &std::path::Path, language: &str, configuration: LspConfiguration) -> EnginePool {
        let key = LspProcessKey::named("test");
        EnginePool::new(
            root,
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([(language.to_owned(), key)]),
        )
    }

    #[test]
    fn exact_name_range_ignores_attributes_comments_and_strings() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source =
            "#[beacon]\n/// beacon\npub fn /* beacon */ beacon() { let text = \"beacon\"; }\n";
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let file = reads
            .index()
            .file(&ProjectPath::new("lib.rs")?)
            .expect("indexed file");
        let declaration = file
            .syntax()
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "beacon")
            .expect("declaration");
        let offset = declaration_name_offset(file, declaration)?;
        assert_eq!(offset, source.find("beacon()").expect("name token"));
        let mut invalid = declaration.clone();
        invalid.name_range.as_mut().expect("name range").end = u64::MAX;
        assert!(declaration_name_offset(file, &invalid).is_err());
        invalid.name_range = None;
        assert!(declaration_name_offset(file, &invalid).is_err());
        Ok(())
    }

    #[test]
    fn reference_mapping_retains_cross_file_callers_and_deduplicates() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("caller.rs"),
            "pub fn caller() { beacon(); beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let seed = symbol(&reads, "beacon");
        let target = rift_core::SymbolId::new(seed.0.clone())?;
        let own = location(directory.path(), "lib.rs", 0, 7, 13);
        let caller = location(directory.path(), "caller.rs", 0, 18, 24);
        let other = location(directory.path(), "caller.rs", 0, 28, 34);
        let canonical = location(reads.index().root(), "caller.rs", 0, 18, 24);
        let report = ReferenceReport {
            locations: Ok(vec![own, caller.clone(), other, caller, canonical]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let edges = map_references(&reads, &target, report, directory.path())?;
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relationship.from, symbol(&reads, "caller"));
        assert_eq!(edges[0].relationship.to, seed);
        assert_eq!(
            edges[0].relationship.facets,
            vec![rift_protocol::read::RelationshipFacet::References]
        );
        Ok(())
    }

    #[test]
    fn reference_mapping_refuses_invalid_ranges_and_location_overflow() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let seed = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        for (start, end) in [(80, 81), (10, 2)] {
            let report = ReferenceReport {
                locations: Ok(vec![location(directory.path(), "lib.rs", 0, start, end)]),
                encoding: PositionEncoding::Utf16,
                full: true,
            };
            assert!(map_references(&reads, &seed, report, directory.path()).is_err());
        }
        let overflow = ReferenceReport {
            locations: Err(super::TRAVERSAL_NODES_MAX + 1),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        assert!(map_references(&reads, &seed, overflow, directory.path()).is_err());
        Ok(())
    }

    #[test]
    fn reference_mapping_skips_unindexed_files_and_refuses_non_file_uris() -> TestResult {
        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let report = ReferenceReport {
            locations: Ok(vec![
                location(outside.path(), "lib.rs", 0, 0, 1),
                location(directory.path(), "absent.rs", 0, 0, 1),
            ]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        assert!(map_references(&reads, &target, report, directory.path())?.is_empty());
        let mut refused = location(directory.path(), "lib.rs", 0, 0, 1);
        refused.uri = "https://example.com/lib.rs".parse()?;
        let report = ReferenceReport {
            locations: Ok(vec![refused]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let error =
            map_references(&reads, &target, report, directory.path()).expect_err("scheme refused");
        assert!(error.detail().contains("scheme_refused"), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn reference_sources_require_indexed_declaration_name_ranges() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("settings.toml"), "beacon = 7\n")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(json!({"command":"/refused-engine"}))?;
        let engines = pool(directory.path(), "toml", configuration);
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let file = reads
            .index()
            .file(&ProjectPath::new("settings.toml")?)
            .expect("indexed file");
        assert!(
            file.syntax()
                .symbols()
                .iter()
                .all(|symbol| symbol.name_range.is_none())
        );
        assert!(
            super::resolve_symbol_references(&reads, &engines, &target)
                .await?
                .is_none()
        );
        let absent = rift_core::SymbolId::new("rift://symbol/toml/absent.toml/beacon")?;
        assert!(
            super::resolve_symbol_references(&reads, &engines, &absent)
                .await?
                .is_none()
        );
        assert_eq!(
            engines.state_for_key(&LspProcessKey::named("test")),
            Some(rift_protocol::workspace::LspState::Stopped)
        );
        Ok(())
    }

    #[tokio::test]
    async fn embedded_references_resolve_two_incoming_depths() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("main.py"),
            "def beacon() -> int:\n    return 7\n\ndef caller() -> int:\n    return beacon()\n\ndef outer() -> int:\n    return caller()\n",
        )?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(
            json!({"embedded":"ty","retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}}),
        )?;
        let engines = pool(directory.path(), "python", configuration);
        let mut params = request(&symbol(&reads, "beacon"));
        params.traversal.as_mut().expect("traversal").depth = 2;
        let result = resolve_engine_references(&reads, &engines, &params).await;
        engines.shutdown().await;
        let references = result?;
        let seed = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let caller = rift_core::SymbolId::new(symbol(&reads, "caller").0)?;
        assert_eq!(
            references.incoming(&seed)[0].relationship.from.0,
            caller.as_str()
        );
        assert_eq!(
            references.incoming(&caller)[0].relationship.from,
            symbol(&reads, "outer")
        );
        let answer = reads.search_with_references(&params, &[], &references)?;
        assert_eq!(answer.results.len(), 2);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn engine_reference_locations_are_bounded_before_deduplication() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let count = super::TRAVERSAL_NODES_MAX + 1;
        let locations = vec![location(directory.path(), "lib.rs", 0, 7, 13); count];
        let responses = (1..=2)
            .map(|id| {
                let body =
                    serde_json::to_string(&json!({"jsonrpc":"2.0","id":id,"result":locations}))?;
                Ok(format!("Content-Length: {}\r\n\r\n{body}", body.len()))
            })
            .collect::<Result<Vec<_>, serde_json::Error>>()?;
        let mut fixture = super::process_lifecycle::retrying(
            super::process_lifecycle::answers(&responses, &["rust"]),
            2,
        );
        let rift_protocol::configuration::CommandInput::ProgramAndArguments(command) = fixture
            .configuration
            .command
            .as_mut()
            .expect("process command")
        else {
            panic!("fixture uses arguments");
        };
        let gate = directory.path().join("finish.gate");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&gate)
                .status()?
                .success()
        );
        let mut release = fs::OpenOptions::new().read(true).write(true).open(&gate)?;
        let script = directory.path().join("responses.sh");
        let waiting = command[2].replace("sleep 0.2", &format!("read -r _ < '{}'", gate.display()));
        fs::write(&script, waiting)?;
        command.truncate(1);
        command.push(script.to_string_lossy().into_owned());
        let engines = pool(directory.path(), "rust", fixture.configuration);
        let result =
            resolve_engine_references(&reads, &engines, &request(&symbol(&reads, "beacon"))).await;
        std::io::Write::write_all(&mut release, b"done\n")?;
        engines.shutdown().await;
        let error = result.expect_err("oversized engine answer");
        assert!(
            matches!(error.fault(), crate::ReadFault::Unavailable { .. }),
            "{error:?}"
        );
        assert!(
            error
                .detail()
                .contains(&format!("reference locations {count}")),
            "{error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn reference_edge_bound_applies_across_engine_requests() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("main.py"),
            "def beacon() -> int:\n    return 7\n\ndef caller() -> int:\n    return beacon()\n",
        )?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let prior = rift_core::SymbolId::new(symbol(&reads, "caller").0)?;
        let report = ReferenceReport {
            locations: Ok(vec![location(directory.path(), "main.py", 4, 11, 17)]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let edge = map_references(&reads, &target, report, directory.path())?.remove(0);
        let prior_edges = (0..super::TRAVERSAL_NODES_MAX)
            .map(|index| {
                let mut edge = edge.clone();
                edge.relationship.from =
                    SymbolId(format!("rift://symbol/python/prior.py/caller_{index}"));
                edge.relationship.to = SymbolId(prior.as_str().to_owned());
                edge
            })
            .collect();
        let mut references =
            EngineReferences::from_incoming(BTreeMap::from([(prior.clone(), prior_edges)]));
        let configuration = serde_json::from_value(
            json!({"embedded":"ty","retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}}),
        )?;
        let engines = pool(directory.path(), "python", configuration);
        let mut requested = std::collections::BTreeSet::from([prior]);
        let result = super::extend_references(
            &reads,
            &engines,
            &mut references,
            vec![target.clone()],
            &mut requested,
        )
        .await;
        engines.shutdown().await;
        let error = result.expect_err("cumulative edge bound");
        assert!(
            error
                .detail()
                .contains("reference edges exceed the traversal node bound"),
            "{error}"
        );
        assert!(references.incoming(&target).is_empty());
        Ok(())
    }

    #[test]
    fn references_reject_another_publication() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let references = EngineReferences {
            revision: Some("other".to_owned()),
            ..EngineReferences::default()
        };
        assert!(
            reads
                .search_with_references(&request(&symbol(&reads, "beacon")), &[], &references)
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_selected_engine_preserves_indexed_traversal() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\npub fn caller() { beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let params = request(&symbol(&reads, "beacon"));
        let engines = EnginePool::new(directory.path(), BTreeMap::new(), BTreeMap::new());
        assert!(!super::uses_engine_references(&reads, &engines, &params)?);
        let references = resolve_engine_references(&reads, &engines, &params).await?;
        assert!(references.is_empty());
        assert_eq!(
            reads.search(&params, &[])?,
            reads.search_with_references(&params, &[], &references)?
        );
        Ok(())
    }

    #[tokio::test]
    async fn configured_engine_failure_preserves_its_error() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(
            json!({"command":"/refused-engine","retry":{"attempts":1},"restart":{"attempts":0}}),
        )?;
        let engines = pool(directory.path(), "rust", configuration);
        let result =
            resolve_engine_references(&reads, &engines, &request(&symbol(&reads, "beacon"))).await;
        engines.shutdown().await;
        let error = result.expect_err("absolute program is refused");
        assert!(matches!(error.fault(), crate::ReadFault::Engine(_)));
        assert!(error.detail().contains("absolute"), "{}", error.detail());
        Ok(())
    }

    #[tokio::test]
    async fn embedded_engine_resolves_cross_file_reference_without_writes() -> TestResult {
        let directory = tempfile::tempdir()?;
        let declaration = "def beacon() -> int:\n    return 7\n";
        let caller = "from helper import beacon\n\ndef caller() -> int:\n    return beacon()\n";
        fs::write(directory.path().join("helper.py"), declaration)?;
        fs::write(directory.path().join("main.py"), caller)?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(
            json!({"embedded":"ty","retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}}),
        )?;
        let engines = pool(directory.path(), "python", configuration);
        let params = request(&symbol(&reads, "beacon"));
        let resolved = resolve_engine_references(&reads, &engines, &params).await;
        engines.shutdown().await;
        let references = resolved?;
        assert!(
            !references.is_empty(),
            "engine must contribute the cross-file reference"
        );
        let answer = reads.search_with_references(&params, &[], &references)?;
        assert!(
            answer
                .results
                .iter()
                .any(|hit| hit.path.as_ref().is_some_and(|path| path.0 == "main.py"))
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("helper.py"))?,
            declaration
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("main.py"))?,
            caller
        );
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn absent_references_capability_preserves_indexed_results() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\npub fn caller() { beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let params = request(&symbol(&reads, "beacon"));
        let mut fixture = super::process_lifecycle::answers(&[], &["rust"]);
        let rift_protocol::configuration::CommandInput::ProgramAndArguments(command) = fixture
            .configuration
            .command
            .as_mut()
            .expect("process command")
        else {
            panic!("fixture uses arguments");
        };
        command[2] = command[2].replace("referencesProvider\":true", "referencesProvider\":null");
        // true and null have identical byte lengths, so the fixture's frame stays valid.
        let engines = pool(directory.path(), "rust", fixture.configuration);
        let result = resolve_engine_references(&reads, &engines, &params).await;
        engines.shutdown().await;
        let references = result?;
        assert!(references.is_empty());
        assert_eq!(
            reads.search(&params, &[])?,
            reads.search_with_references(&params, &[], &references)?
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn engine_refusal_remains_a_typed_read_failure() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let response = super::process_lifecycle::refused_response(1, -32602, "references refused");
        let mut fixture = super::process_lifecycle::answers(&[response], &["rust"]);
        fixture.retry.attempts = 1;
        let engines = pool(directory.path(), "rust", fixture.configuration);
        let result =
            resolve_engine_references(&reads, &engines, &request(&symbol(&reads, "beacon"))).await;
        engines.shutdown().await;
        let error = result.expect_err("engine refused references");
        assert!(matches!(error.fault(), crate::ReadFault::Engine(_)));
        assert!(
            error.detail().contains("references refused"),
            "{}",
            error.detail()
        );
        Ok(())
    }
    #[tokio::test]
    async fn invalid_search_refuses_before_selected_engine_launch() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(json!({"command":"/refused-engine"}))?;
        let engines = pool(directory.path(), "rust", configuration);
        let base = request(&symbol(&reads, "beacon"));
        for invalid in [
            json!({"scope":"dependencies"}),
            json!({"query":""}),
            json!({"limit":0}),
            json!({"paths":{"include":["["]}}),
        ] {
            let mut value = serde_json::to_value(&base)?;
            value
                .as_object_mut()
                .expect("request")
                .extend(invalid.as_object().expect("overrides").clone());
            let params = serde_json::from_value(value)?;
            let expected = reads.search(&params, &[]).expect_err("invalid search");
            let error = resolve_engine_references(&reads, &engines, &params)
                .await
                .expect_err("invalid search before engine");
            assert_eq!(error.descriptor().code(), expected.descriptor().code());
            assert_eq!(error.detail(), expected.detail());
        }
        engines.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn resolved_empty_references_work_without_binding() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("helper.py"),
            "def beacon() -> int:\n    return 7\n",
        )?;
        let binding = rift_protocol::configuration::BindingConfiguration {
            enabled: false,
            ..Default::default()
        };
        let reads = ReadService::build_with_languages(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &rift_core::LanguageFileSelections::default(),
            rift_index::BindingPolicy::from(&binding),
            HistoryConfiguration::default(),
            rift_protocol::dependencies::DependenciesConfiguration::default(),
        )?;
        let configuration = serde_json::from_value(
            json!({"embedded":"ty","retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}}),
        )?;
        let engines = pool(directory.path(), "python", configuration);
        let params = request(&symbol(&reads, "beacon"));
        let result = resolve_engine_references(&reads, &engines, &params).await;
        engines.shutdown().await;
        let references = result?;
        assert!(references.is_empty());
        assert!(references.resolved(&params.traversal.as_ref().expect("traversal").seed));
        assert!(
            reads
                .search_with_references(&params, &[], &references)?
                .results
                .is_empty()
        );
        Ok(())
    }
    #[test]
    fn engine_eligibility_preserves_indexed_only_searches() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(json!({"command":"/refused-engine"}))?;
        let engines = pool(directory.path(), "rust", configuration);
        let base = request(&symbol(&reads, "beacon"));
        assert!(super::uses_engine_references(&reads, &engines, &base)?);
        let mut outgoing = base.clone();
        outgoing.traversal.as_mut().expect("traversal").direction =
            rift_protocol::read::TraversalDirection::Outgoing;
        let mut calls = base.clone();
        calls.traversal.as_mut().expect("traversal").facets =
            vec![rift_protocol::read::RelationshipFacet::Calls];
        let mut files = base;
        files.target = rift_protocol::read::SearchParamsTarget::File;
        for params in [outgoing, calls, files] {
            assert!(!super::uses_engine_references(&reads, &engines, &params)?);
        }
        assert_eq!(
            engines.state_for_key(&LspProcessKey::named("test")),
            Some(rift_protocol::workspace::LspState::Stopped)
        );
        Ok(())
    }

    #[test]
    fn depth_two_reaches_engine_configured_caller_of_unconfigured_seed() -> TestResult {
        let store = crate::traversal::tests::call_graph_store();
        let seed = rift_core::SymbolId::new("rift://symbol/rust/lib.rs/leaf")?;
        let caller = rift_core::SymbolId::new("rift://symbol/rust/lib.rs/branch_a")?;
        let mut traversal = request(&SymbolId(seed.as_str().to_owned()))
            .traversal
            .expect("traversal");
        traversal.facets.clear();
        let selected = |identity: &rift_core::SymbolId| identity == &caller;
        assert!(!selected(&seed));
        assert!(!super::reachable_reference_source(
            &store, &seed, &traversal, selected
        ));
        traversal.depth = 2;
        assert!(super::reachable_reference_source(
            &store, &seed, &traversal, selected
        ));
        traversal.facets = vec![rift_protocol::read::RelationshipFacet::References];
        assert!(!super::reachable_reference_source(
            &store, &seed, &traversal, selected
        ));
        Ok(())
    }
}
