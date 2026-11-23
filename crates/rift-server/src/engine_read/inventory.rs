//! File groups for bounded engine edge inventory reads.

mod calls;
mod targets;
mod types;
pub use targets::PackageEdge;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use lsp_types::{CallHierarchyOutgoingCall, Location, Position};
use rift_core::{ProjectPath, SymbolId as CoreSymbolId};
use rift_error::RiftError;
use rift_lsp::{EngineSession, LineIndex, PositionEncoding};
use rift_protocol::read::{
    EdgeKind, EdgeProvider, ReadWarning, SymbolId, SymbolKind, TraversalDirection,
};
use tokio::time::Instant;

use super::{
    CallGraphSource, CalleeReport, CapturedCalls, GraphCalls, GraphReferences, ReferenceReport,
    ReferenceTarget, all_edge_declarations, graph_call_coverage, map_graph_calls,
};
use crate::engine::{EngineWalkDeadline, OutgoingAnswer};
use crate::traversal::TRAVERSAL_SYMBOLS_MAX;
use crate::{CalleeRoots, EnginePool, ReadService};

/// The edge kinds an engine inventory reads from each declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineInventorySelection {
    /// Read outgoing calls.
    pub calls: bool,
    /// Read incoming reference locations.
    pub references: bool,
    /// Resolve inferred type definitions and inheritance.
    pub types: rift_search::sparql::TypeSelection,
    /// Keep only references held by a canonical declaration.
    pub enclosing: bool,
    /// Physical reference occurrences requested by this inventory.
    pub occurrences: Option<rift_search::sparql::OccurrenceSelection>,
    /// Most requests sent before the session reads their answers.
    pub batch_inputs: usize,
    /// Most records retained from one file.
    pub records_max: usize,
}

/// One indexed file and the declaration positions its engine reads.
#[derive(Debug)]
pub struct EngineInventoryFile {
    language: rift_protocol::read::Language,
    target: ReferenceTarget,
    positions: Vec<InventoryPosition>,
    seeds: BTreeSet<SymbolId>,
    call_sites: Vec<InventoryCallSite>,
    target_sites: Vec<targets::Site>,
    has_static_calls: bool,
}

#[derive(Clone, Debug)]
struct InventoryPosition {
    symbol: SymbolId,
    utf8: Position,
    utf16: Position,
    declared_type: Option<Vec<(Position, Position)>>,
    type_declaration: bool,
    implementation_kind: Option<EdgeKind>,
}

impl ReadService {
    /// Groups canonical declarations by file without copying each declaration's source.
    ///
    /// # Errors
    /// Returns an identity, source position, or declaration coverage failure.
    pub fn engine_inventory_files(
        &self,
        seeds: &[SymbolId],
        selection: EngineInventorySelection,
    ) -> Result<Vec<EngineInventoryFile>, RiftError> {
        let mut grouped = BTreeMap::<ProjectPath, EngineInventoryFile>::new();
        let mut lines = BTreeMap::new();
        let mut declared_types = BTreeMap::new();
        for seed in seeds {
            let identity = CoreSymbolId::new(seed.as_str())?;
            for declaration in all_edge_declarations(self, &identity)? {
                let file = declaration.file;
                let symbol = declaration.symbol;
                let group = inventory_file(&mut grouped, file, self.index().root())?;
                group.seeds.insert(seed.clone());
                let Some(range) = symbol.name_range else {
                    continue;
                };
                let index = lines
                    .entry(file.path().clone())
                    .or_insert_with(|| LineIndex::new(file.source()));
                let offset = usize::try_from(range.start).map_err(|_| graph_call_coverage())?;
                if selection.types.definitions && !declared_types.contains_key(file.path()) {
                    declared_types.insert(file.path().clone(), types::declared_types(file)?);
                }
                let declared_type = declared_types
                    .get(file.path())
                    .and_then(|types| types.get(&range.start))
                    .map(|offsets| {
                        offsets
                            .iter()
                            .map(|offset| {
                                Ok((
                                    index.position(PositionEncoding::Utf8, *offset)?,
                                    index.position(PositionEncoding::Utf16, *offset)?,
                                ))
                            })
                            .collect::<Result<Vec<_>, RiftError>>()
                    })
                    .transpose()?;
                group.positions.push(InventoryPosition {
                    symbol: seed.clone(),
                    utf8: index.position(PositionEncoding::Utf8, offset)?,
                    utf16: index.position(PositionEncoding::Utf16, offset)?,
                    declared_type,
                    type_declaration: symbol.kinds.contains(&SymbolKind::Type),
                    implementation_kind: super::heritage_edges::implementation_kind(symbol),
                });
            }
        }
        let mut files = Vec::with_capacity(grouped.len());
        for mut group in grouped.into_values() {
            if selection.calls {
                let file = self
                    .index()
                    .file(&group.target.path)
                    .ok_or_else(graph_call_coverage)?;
                group.call_sites = inventory_call_sites(self, file, &group.seeds)?;
            }
            let file = self
                .index()
                .file(&group.target.path)
                .ok_or_else(graph_call_coverage)?;
            group.target_sites = targets::sites(self, file, &group.seeds, selection)?;
            files.push(group);
        }
        Ok(files)
    }
}

pub(super) async fn outgoing_calls(
    reads: &ReadService,
    engines: &EnginePool,
    roots: &CalleeRoots,
    seed: &SymbolId,
    deadline: Instant,
) -> Result<GraphCalls, RiftError> {
    let selection = EngineInventorySelection {
        calls: true,
        references: false,
        types: rift_search::sparql::TypeSelection::default(),
        enclosing: true,
        occurrences: None,
        batch_inputs: rift_lsp::correlation::PENDING_REQUESTS_MAX,
        records_max: super::TRAVERSAL_SYMBOLS_MAX,
    };
    let mut answer: Option<GraphCalls> = None;
    let mut remaining = super::TRAVERSAL_SYMBOLS_MAX;
    let sources = super::DocumentSources::default();
    for file in reads.engine_inventory_files(std::slice::from_ref(seed), selection)? {
        let raw = match file.read(engines, (selection, &sources), deadline).await {
            Ok(raw) => raw,
            Err(error)
                if super::graph_engine_method_missing(&error)
                    || error.slug()
                        == rift_error::errors::server::search_coverage_unavailable::SLUG
                    || error.slug() == rift_error::errors::lsp::engine_analyzing::SLUG =>
            {
                let mut captured =
                    super::captured_graph_calls(reads, seed, TraversalDirection::Outgoing)?;
                captured.complete = false;
                captured.edge_warnings.push(super::engine_analysis_warning(
                    file.language,
                    error.detail(),
                ));
                return Ok(captured);
            }
            Err(error) => return Err(error),
        };
        let Some(raw) = raw else { continue };
        let inventory = raw
            .resolve(reads, roots)?
            .confirm_calls(reads, engines, deadline)
            .await;
        for (caller, mut calls) in inventory.calls {
            if &caller != seed {
                continue;
            }
            remaining = remaining
                .checked_sub(calls.record_count())
                .ok_or_else(graph_call_coverage)?;
            calls
                .edge_warnings
                .extend(inventory.warnings.iter().cloned());
            match &mut answer {
                Some(answer) => answer.merge(calls),
                None => answer = Some(calls),
            }
        }
    }
    match answer {
        Some(answer) => Ok(answer),
        None => super::captured_graph_calls(reads, seed, TraversalDirection::Outgoing),
    }
}

fn inventory_file<'group>(
    grouped: &'group mut BTreeMap<ProjectPath, EngineInventoryFile>,
    file: &rift_index::IndexedFile,
    root: &std::path::Path,
) -> Result<&'group mut EngineInventoryFile, RiftError> {
    match grouped.entry(file.path().clone()) {
        std::collections::btree_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
        std::collections::btree_map::Entry::Vacant(entry) => {
            Ok(entry.insert(EngineInventoryFile {
                language: file.syntax().language().clone(),
                target: ReferenceTarget::file(root, root, file)?,
                positions: Vec::new(),
                seeds: BTreeSet::new(),
                call_sites: Vec::new(),
                target_sites: Vec::new(),
                has_static_calls: file.syntax().static_facts().is_some(),
            }))
        }
    }
}

/// An engine's file answer, waiting for source location conversion on the worker pool.
#[derive(Debug)]
pub struct EngineInventoryAnswer {
    provider: EdgeProvider,
    path: ProjectPath,
    workspace_root: PathBuf,
    selection: EngineInventorySelection,
    encoding: PositionEncoding,
    calls: Vec<InventoryCalls>,
    references: Vec<(SymbolId, Vec<Location>)>,
    reference_definitions: super::ReferenceDefinitions,
    types: Vec<(SymbolId, Vec<Location>)>,
    heritage: Vec<InventoryHeritage>,
    definitions: Vec<InventoryDefinition>,
    targets: Vec<targets::Answer>,
    definition_callers: BTreeSet<SymbolId>,
    warnings: Vec<ReadWarning>,
}

/// Calls and references mapped to canonical local declarations.
#[derive(Debug)]
pub struct EngineInventory {
    /// Outgoing calls for each declaration the engine prepared.
    pub calls: Vec<(SymbolId, GraphCalls)>,
    /// Incoming references for each declaration the engine read.
    pub references: Vec<(SymbolId, GraphReferences)>,
    /// Inferred type edges between canonical declarations.
    pub types: Vec<rift_protocol::index::StoredEdge>,
    /// Inheritance and implementation edges between canonical declarations.
    pub heritage: Vec<rift_protocol::index::StoredEdge>,
    /// Readiness warnings carried by this file's engine answer.
    pub warnings: Vec<ReadWarning>,
    /// External reference, type and inheritance targets awaiting package placement.
    pub packages: Vec<PackageEdge>,
}

impl EngineInventory {
    /// Restores authored calls independently confirmed by engine references.
    /// Original disagreements and incomplete coverage remain in each answer.
    ///
    /// # Cancel safety
    /// Dropping the future cancels the current exchange and retains no inventory.
    pub async fn confirm_calls(
        mut self,
        reads: &ReadService,
        engines: &EnginePool,
        deadline: Instant,
    ) -> Self {
        let mut targets = super::call_evidence::TargetReferences::default();
        for (seed, calls) in &mut self.calls {
            *calls = super::call_evidence::confirm_reference_sites(
                reads,
                engines,
                seed,
                std::mem::take(calls),
                &mut targets,
                deadline,
            )
            .await;
        }
        self
    }
}

impl EngineInventoryFile {
    /// Reads this file in one exchange, with bounded groups of requests in flight.
    ///
    /// # Errors
    /// Returns an engine exchange, admission, or response bound failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves the session's pending answers for its next exchange.
    pub async fn read(
        &self,
        engines: &EnginePool,
        (selection, sources): (EngineInventorySelection, &super::DocumentSources),
        deadline: impl Into<EngineWalkDeadline>,
    ) -> Result<Option<EngineInventoryAnswer>, RiftError> {
        let sources = sources.clone();
        let Some(slot) = engines.engine_for(&self.language) else {
            return Ok(None);
        };
        let positions = self.positions.clone();
        let path = self.target.path.clone();
        let sites = self.call_sites.clone();
        let target_sites = self.target_sites.clone();
        let callers = self.seeds.clone();
        let has_static_calls = self.has_static_calls;
        let answered = slot
            .request_outgoing(
                self.target.opening(),
                move |session| {
                    let positions = positions.clone();
                    let path = path.clone();
                    let sites = sites.clone();
                    let target_sites = target_sites.clone();
                    let callers = callers.clone();
                    let sources = sources.clone();
                    Box::pin(async move {
                        let mut report =
                            read_positions(session, &path, &positions, (selection, &sources))
                                .await?;
                        if selection.calls
                            && has_static_calls
                            && session.capabilities().definitions.definition
                        {
                            read_definitions(
                                session,
                                &path,
                                &sites,
                                callers,
                                selection,
                                &mut report,
                            )
                            .await?;
                        }
                        targets::read(
                            session,
                            &path,
                            &target_sites,
                            selection.batch_inputs,
                            &mut report,
                        )
                        .await?;
                        Ok(Some(report))
                    })
                },
                self.target.closing(),
                deadline,
            )
            .await?;
        let (report, warnings) = match answered {
            OutgoingAnswer::Ready(report) => (report, Vec::new()),
            OutgoingAnswer::Unconfirmed(report) => (
                report,
                vec![super::readiness_unconfirmed_warning(&BTreeSet::from([
                    slot.name().to_owned(),
                ]))],
            ),
            OutgoingAnswer::Unsettled { attempts } => {
                return Err(rift_error::errors::lsp::engine_analyzing()
                    .attempts(attempts)
                    .error());
            }
            OutgoingAnswer::Unprepared => return Ok(None),
        };
        Ok(Some(EngineInventoryAnswer {
            provider: slot.edge_provider(),
            path: self.target.path.clone(),
            workspace_root: slot.workspace_root().to_path_buf(),
            selection,
            encoding: report.encoding,
            calls: report.calls,
            references: report.references,
            reference_definitions: report.reference_definitions,
            types: report.types,
            heritage: report.heritage,
            definitions: report.definitions,
            targets: report.targets,
            definition_callers: report.definition_callers,
            warnings,
        }))
    }
}

#[derive(Debug)]
struct InventoryCalls {
    symbol: SymbolId,
    calls: Option<Vec<CallHierarchyOutgoingCall>>,
}

struct InventoryReport {
    remaining: usize,
    encoding: PositionEncoding,
    calls: Vec<InventoryCalls>,
    references: Vec<(SymbolId, Vec<Location>)>,
    reference_definitions: super::ReferenceDefinitions,
    types: Vec<(SymbolId, Vec<Location>)>,
    heritage: Vec<InventoryHeritage>,
    definitions: Vec<InventoryDefinition>,
    targets: Vec<targets::Answer>,
    definition_callers: BTreeSet<SymbolId>,
}

impl InventoryReport {
    fn locations(
        &mut self,
        positions: &[InventoryPosition],
        answers: Vec<Vec<Location>>,
    ) -> Result<Vec<(SymbolId, Vec<Location>)>, RiftError> {
        let mut locations = Vec::with_capacity(positions.len());
        for (position, answer) in positions.iter().zip(answers) {
            self.charge(answer.len().saturating_add(1))?;
            locations.push((position.symbol.clone(), answer));
        }
        Ok(locations)
    }

    fn supertypes(
        &mut self,
        seed: &SymbolId,
        selected: lsp_types::SymbolKind,
        neighbors: Vec<lsp_types::TypeHierarchyItem>,
        selection: EngineInventorySelection,
    ) -> Result<(), RiftError> {
        self.charge(neighbors.len())?;
        for neighbor in neighbors {
            let kind = if selected != lsp_types::SymbolKind::INTERFACE
                && neighbor.kind == lsp_types::SymbolKind::INTERFACE
            {
                EdgeKind::Implements
            } else {
                EdgeKind::Extends
            };
            if selected_heritage(selection, kind) {
                self.heritage.push(InventoryHeritage {
                    seed: seed.clone(),
                    kind,
                    direction: TraversalDirection::Outgoing,
                    location: Location {
                        uri: neighbor.uri,
                        range: neighbor.selection_range,
                    },
                });
            }
        }
        Ok(())
    }

    fn charge(&mut self, records: usize) -> Result<(), RiftError> {
        self.remaining = self
            .remaining
            .checked_sub(records)
            .ok_or_else(graph_call_coverage)?;
        Ok(())
    }
}

#[derive(Debug)]
struct InventoryHeritage {
    seed: SymbolId,
    kind: EdgeKind,
    direction: TraversalDirection,
    location: Location,
}

async fn read_positions(
    session: &mut EngineSession,
    path: &ProjectPath,
    positions: &[InventoryPosition],
    (selection, sources): (EngineInventorySelection, &super::DocumentSources),
) -> Result<InventoryReport, RiftError> {
    let encoding = session.capabilities().position_encoding;
    let mut report = InventoryReport {
        remaining: selection.records_max,
        encoding,
        calls: Vec::new(),
        references: Vec::new(),
        reference_definitions: BTreeMap::new(),
        types: Vec::new(),
        heritage: Vec::new(),
        definitions: Vec::new(),
        targets: Vec::new(),
        definition_callers: BTreeSet::new(),
    };
    let maximum = selection
        .batch_inputs
        .clamp(1, rift_lsp::correlation::PENDING_REQUESTS_MAX);
    for chunk in positions.chunks(maximum) {
        let requests: Vec<_> = chunk
            .iter()
            .map(|position| {
                (
                    path.clone(),
                    match encoding {
                        PositionEncoding::Utf8 => position.utf8,
                        PositionEncoding::Utf16 => position.utf16,
                    },
                )
            })
            .collect();
        if selection.references && session.capabilities().references {
            let answers = session.references_batch(&requests).await?;
            let locations = report.locations(chunk, answers)?;
            report.references.extend(locations);
        }
        if selection.types.definitions {
            types::read(session, path, chunk, &requests, maximum, &mut report).await?;
        }
        if selection.types.extends || selection.types.implements {
            read_heritage(session, chunk, &requests, selection, &mut report).await?;
        }
        if selection.calls && session.capabilities().call_hierarchy {
            read_calls(session, path, chunk, &requests, maximum, &mut report).await?;
        }
    }
    let locations: Vec<_> = report
        .references
        .iter()
        .flat_map(|(_, locations)| locations.iter().cloned())
        .collect();
    report.reference_definitions =
        super::reference_definitions(session, &locations, (maximum, report.remaining), sources)
            .await?;
    report.charge(report.reference_definitions.values().map(Vec::len).sum())?;
    Ok(report)
}

/// Emits only fixture roles and state for the exact Intel inventory diagnosis.
fn trace_inventory_phase(
    session: &EngineSession,
    path: &ProjectPath,
    phase: &'static str,
    complete: bool,
) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| {
        std::env::var_os("RIFT_DIAG_INVENTORY_PHASES").is_some()
            && std::env::var("NEXTEST_TEST_NAME").as_deref()
                == Ok("test_edge_inventory_reads_cut_at_their_deadline_keep_the_engine")
    }) {
        return;
    }
    let role = match path.as_str() {
        "lib.rs" => "first",
        "one.rs" => "second",
        "two.rs" => "third",
        _ => return,
    };
    rift_tracing::info!(
        component = "engine",
        role,
        phase,
        complete,
        readiness = ?session.walk_readiness(),
        intact = session.is_intact(),
        "inventory batch phase"
    );
}

async fn read_calls(
    session: &mut EngineSession,
    path: &ProjectPath,
    positions: &[InventoryPosition],
    requests: &[(ProjectPath, Position)],
    maximum: usize,
    report: &mut InventoryReport,
) -> Result<(), RiftError> {
    trace_inventory_phase(session, path, "prepare_begin", false);
    let prepared = session.prepare_call_hierarchy_batch(requests).await;
    trace_inventory_phase(session, path, "prepare_end", prepared.is_ok());
    let prepared = prepared?;
    let mut items = Vec::new();
    for ((position, (path, at)), prepared) in positions.iter().zip(requests).zip(prepared) {
        report.charge(1)?;
        report.calls.push(InventoryCalls {
            symbol: position.symbol.clone(),
            calls: (!prepared.is_empty()).then(Vec::new),
        });
        let uri = session.root().document_uri(path)?;
        let accepted: Vec<_> = prepared
            .into_iter()
            .filter(|item| {
                item.uri == uri
                    && item.selection_range.start <= *at
                    && *at < item.selection_range.end
            })
            .collect();
        if items.len().saturating_add(accepted.len()) > TRAVERSAL_SYMBOLS_MAX {
            return Err(graph_call_coverage());
        }
        items.extend(
            accepted
                .into_iter()
                .map(|item| (report.calls.len() - 1, item)),
        );
    }
    for chunk in items.chunks(maximum) {
        trace_inventory_phase(session, path, "outgoing_begin", false);
        let answers = session
            .outgoing_calls_batch(chunk.iter().map(|(_, item)| item.clone()).collect())
            .await;
        trace_inventory_phase(session, path, "outgoing_end", answers.is_ok());
        let answers = answers?;
        for ((position, _), calls) in chunk.iter().zip(answers) {
            report.charge(calls.len())?;
            let stored = report.calls[*position]
                .calls
                .as_mut()
                .ok_or_else(graph_call_coverage)?;
            if stored.len().saturating_add(calls.len()) > TRAVERSAL_SYMBOLS_MAX {
                return Err(graph_call_coverage());
            }
            stored.extend(calls);
        }
    }
    Ok(())
}

impl EngineInventoryAnswer {
    /// Places reference, type and inheritance targets the engine answered outside the
    /// workspace in their package or runtime sources; their warnings join the answer's.
    fn place_targets(
        &mut self,
        reads: &ReadService,
        roots: &CalleeRoots,
    ) -> Result<targets::Resolved, RiftError> {
        let mut targets = targets::Mapping {
            reads,
            roots,
            workspace: &self.workspace_root,
            encoding: self.encoding,
            provider: self.provider,
        }
        .resolve(
            std::mem::take(&mut self.types),
            std::mem::take(&mut self.heritage),
            std::mem::take(&mut self.targets),
        )?;
        self.warnings.append(&mut targets.warnings);
        Ok(targets)
    }

    /// Maps returned locations against the captured index on the bounded worker pool.
    ///
    /// # Errors
    /// Returns invalid engine positions or exceeded occurrence bounds.
    pub fn resolve(
        mut self,
        reads: &ReadService,
        roots: &CalleeRoots,
    ) -> Result<EngineInventory, RiftError> {
        let targets = self.place_targets(reads, roots)?;
        let file = reads
            .index()
            .file(&self.path)
            .ok_or_else(graph_call_coverage)?;
        let held = CallGraphSource {
            provider: self.provider,
            trees: [&self.workspace_root, reads.index().root()],
            source: file.source(),
            unit: crate::read::source_unit_id(&self.path),
            direction: TraversalDirection::Outgoing,
        };
        let mut calls = Vec::new();
        for reported in self.calls {
            let seed = reported.symbol;
            if reported.calls.is_none() && self.definition_callers.contains(&seed) {
                continue;
            }
            let identity = CoreSymbolId::new(seed.as_str())?;
            let captured = CapturedCalls::read(reads, &identity, TraversalDirection::Outgoing);
            let answer = match reported.calls {
                Some(calls) => {
                    let engine = map_graph_calls(
                        reads,
                        roots,
                        &identity,
                        CalleeReport {
                            calls: Ok(calls),
                            encoding: self.encoding,
                        },
                        &held,
                    )?;
                    captured.combine(engine)
                }
                None => captured.answer().unwrap_or_default(),
            };
            calls.push((seed, answer));
        }
        let sites = self
            .definitions
            .iter()
            .map(|definition| {
                (
                    definition.caller.clone(),
                    definition.source.clone(),
                    definition.at,
                )
            })
            .collect();
        let definitions = resolve_definitions(
            reads,
            (roots, &self.workspace_root),
            self.encoding,
            self.definition_callers,
            self.definitions,
            self.provider,
            !file.syntax().has_errors()
                && file
                    .syntax()
                    .static_facts()
                    .is_some_and(|facts| facts.unsupported_bindings().is_empty()),
        )?;
        let calls = calls::merge_calls(calls, definitions, sites);
        let references = resolve_references(
            reads,
            &self.workspace_root,
            (self.encoding, self.provider),
            self.selection,
            self.references,
            self.reference_definitions,
        )?;
        let types = reads.inventory_type_edges(
            &self.workspace_root,
            self.encoding,
            targets.types,
            self.provider,
        )?;
        let heritage = resolve_heritage(
            reads,
            &self.workspace_root,
            self.encoding,
            targets.heritage,
            self.provider,
        )?;
        Ok(EngineInventory {
            packages: targets.packages,
            heritage,
            types,
            calls,
            references,
            warnings: self.warnings,
        })
    }
}

fn resolve_references(
    reads: &ReadService,
    root: &std::path::Path,
    (encoding, provider): (PositionEncoding, EdgeProvider),
    selection: EngineInventorySelection,
    reports: Vec<(SymbolId, Vec<Location>)>,
    definitions: super::ReferenceDefinitions,
) -> Result<Vec<(SymbolId, GraphReferences)>, RiftError> {
    let reference_definitions = std::sync::Arc::new(definitions);
    let mut mapping = super::ReferenceMapping::new(reads, root, encoding, provider)?;
    let mut references = Vec::new();
    for (seed, locations) in reports {
        let mut engine = super::map_inventory_references_with_mapping(
            &seed,
            ReferenceReport {
                locations: Ok(locations),
                definitions: std::sync::Arc::clone(&reference_definitions),
                encoding,
                full: true,
            },
            selection.enclosing,
            &mut mapping,
        )?;
        if selection.occurrences.is_none() {
            let mut callers = BTreeSet::new();
            engine
                .occurrences
                .retain(|occurrence| callers.insert(occurrence.enclosing.clone()));
        }
        references.push((seed, engine));
    }
    Ok(references)
}

impl ReadService {
    fn inventory_type_edges(
        &self,
        root: &std::path::Path,
        encoding: PositionEncoding,
        reported: Vec<(SymbolId, Vec<Location>)>,
        provider: EdgeProvider,
    ) -> Result<Vec<rift_protocol::index::StoredEdge>, RiftError> {
        let roots = [
            rift_lsp::TreeRoot::new(root)?,
            rift_lsp::TreeRoot::new(self.index().root())?,
        ];
        let mut edges = BTreeSet::new();
        for (seed, locations) in reported {
            let targets = self.inventory_type_targets(&roots, encoding, locations)?;
            edges.extend(
                targets
                    .into_iter()
                    .filter(|target| target != &seed)
                    .map(|target| (seed.clone(), target)),
            );
        }
        Ok(edges
            .into_iter()
            .map(|(from, to)| {
                super::heritage_edges::edge_protocol(from, to, EdgeKind::HasType, provider)
            })
            .collect())
    }

    fn inventory_type_targets(
        &self,
        roots: &[rift_lsp::TreeRoot; 2],
        encoding: PositionEncoding,
        locations: Vec<Location>,
    ) -> Result<BTreeSet<SymbolId>, RiftError> {
        let mut targets = BTreeSet::new();
        for location in locations {
            let path = roots[0]
                .project_path(&location.uri)
                .or_else(|_| roots[1].project_path(&location.uri));
            let Ok(path) = path else { continue };
            if let Some(target) =
                super::project_definition(self, &path, (location.range, encoding), true)?
            {
                targets.insert(target);
            }
        }
        Ok(targets)
    }
}

async fn read_heritage(
    session: &mut EngineSession,
    positions: &[InventoryPosition],
    requests: &[(ProjectPath, Position)],
    selection: EngineInventorySelection,
    report: &mut InventoryReport,
) -> Result<(), RiftError> {
    let selected: Vec<_> = positions
        .iter()
        .zip(requests)
        .filter(|(position, _)| position.type_declaration)
        .collect();
    if selected.is_empty() {
        return Ok(());
    }
    let requests: Vec<_> = selected
        .iter()
        .map(|(_, request)| (*request).clone())
        .collect();
    if session.capabilities().edges.type_hierarchy {
        let prepared = session.prepare_type_hierarchy_batch(&requests).await?;
        let mut items = Vec::new();
        for ((position, (path, at)), prepared) in selected.iter().zip(prepared) {
            let uri = session.root().document_uri(path)?;
            position.append_type_items(&uri, *at, prepared, &mut items)?;
        }
        read_supertypes(session, items, selection, report).await?;
    }
    if session.capabilities().edges.implementation && selection.types.implements {
        read_implementations(session, &selected, report).await?;
    }
    Ok(())
}

impl InventoryPosition {
    fn append_type_items(
        &self,
        uri: &lsp_types::Uri,
        at: Position,
        prepared: Vec<lsp_types::TypeHierarchyItem>,
        items: &mut Vec<(SymbolId, lsp_types::TypeHierarchyItem)>,
    ) -> Result<(), RiftError> {
        for item in prepared {
            if items.len() == TRAVERSAL_SYMBOLS_MAX {
                return Err(graph_call_coverage());
            }
            if item.uri == *uri && item.selection_range.start <= at && at < item.selection_range.end
            {
                items.push((self.symbol.clone(), item));
            }
        }
        Ok(())
    }
}

async fn read_implementations(
    session: &mut EngineSession,
    selected: &[(&InventoryPosition, &(ProjectPath, Position))],
    report: &mut InventoryReport,
) -> Result<(), RiftError> {
    // Class implementation results include indirect subclasses. Direct inheritance
    // comes from type hierarchy; implementation requests select interfaces and traits.
    let implementations: Vec<_> = selected
        .iter()
        .filter(|(position, _)| position.implementation_kind == Some(EdgeKind::Implements))
        .collect();
    let requests: Vec<_> = implementations
        .iter()
        .map(|(_, request)| (*request).clone())
        .collect();
    if requests.is_empty() {
        return Ok(());
    }
    let answers = session.implementation_batch(&requests).await?;
    for ((position, _), locations) in implementations.into_iter().zip(answers) {
        report.charge(locations.len())?;
        report
            .heritage
            .extend(locations.into_iter().map(|location| InventoryHeritage {
                seed: position.symbol.clone(),
                kind: EdgeKind::Implements,
                direction: TraversalDirection::Incoming,
                location,
            }));
    }
    Ok(())
}

fn selected_heritage(selection: EngineInventorySelection, kind: EdgeKind) -> bool {
    match kind {
        EdgeKind::Extends => selection.types.extends,
        EdgeKind::Implements => selection.types.implements,
        _ => false,
    }
}

async fn read_supertypes(
    session: &mut EngineSession,
    items: Vec<(SymbolId, lsp_types::TypeHierarchyItem)>,
    selection: EngineInventorySelection,
    report: &mut InventoryReport,
) -> Result<(), RiftError> {
    let maximum = selection
        .batch_inputs
        .clamp(1, rift_lsp::correlation::PENDING_REQUESTS_MAX);
    for chunk in items.chunks(maximum) {
        let answers = session
            .supertype_edges_batch(chunk.iter().map(|(_, item)| item.clone()).collect())
            .await?;
        for ((seed, item), neighbors) in chunk.iter().zip(answers) {
            report.supertypes(seed, item.kind, neighbors, selection)?;
        }
    }
    Ok(())
}

fn resolve_heritage(
    reads: &ReadService,
    workspace_root: &std::path::Path,
    encoding: PositionEncoding,
    heritage: Vec<InventoryHeritage>,
    provider: EdgeProvider,
) -> Result<Vec<rift_protocol::index::StoredEdge>, RiftError> {
    let roots = [
        rift_lsp::TreeRoot::new(workspace_root)?,
        rift_lsp::TreeRoot::new(reads.index().root())?,
    ];
    let mut edges = Vec::new();
    for item in heritage {
        let path = roots[0]
            .project_path(&item.location.uri)
            .or_else(|_| roots[1].project_path(&item.location.uri));
        let Ok(path) = path else { continue };
        let neighbor =
            super::project_definition(reads, &path, (item.location.range, encoding), true)?;
        if item.kind == EdgeKind::Implements && item.direction == TraversalDirection::Incoming {
            super::heritage_edges::edge_implementation(
                reads,
                &CoreSymbolId::new(item.seed.as_str())?,
                &path,
                (item.location.range, encoding),
                neighbor,
                provider,
                &mut edges,
            )?;
            continue;
        }
        if let Some(neighbor) = neighbor {
            if neighbor == item.seed {
                continue;
            }
            let (from, to) = match item.direction {
                TraversalDirection::Outgoing => (item.seed, neighbor),
                TraversalDirection::Incoming => (neighbor, item.seed),
            };
            edges.push(super::heritage_edges::edge_protocol(
                from, to, item.kind, provider,
            ));
        } else if item.direction == TraversalDirection::Incoming {
            super::heritage_edges::edge_impl_header(
                reads,
                &CoreSymbolId::new(item.seed.as_str())?,
                &path,
                (item.location.range, encoding),
                item.kind,
                provider,
                &mut edges,
            )?;
        }
    }
    Ok(edges)
}

#[derive(Clone, Debug)]
struct InventoryCallSite {
    name: String,
    caller: SymbolId,
    source: rift_protocol::read::SourceUnitSpan,
    utf8: Position,
    utf16: Position,
    at: u64,
}

#[derive(Debug)]
struct InventoryDefinition {
    name: String,
    at: u64,
    caller: SymbolId,
    source: rift_protocol::read::SourceUnitSpan,
    locations: Vec<Location>,
}

fn inventory_call_sites(
    reads: &ReadService,
    file: &rift_index::IndexedFile,
    seeds: &BTreeSet<SymbolId>,
) -> Result<Vec<InventoryCallSite>, RiftError> {
    let Some(facts) = file.syntax().static_facts() else {
        return Ok(Vec::new());
    };
    let lines = LineIndex::new(file.source());
    let mut sites = Vec::new();
    for call in facts.calls() {
        let Some(caller) =
            super::edge_enclosing_symbol(reads, file, call.range.start, call.range.end)?
        else {
            continue;
        };
        if !seeds.contains(&caller) {
            continue;
        }
        let start = usize::try_from(call.callee_range.start).map_err(|_| graph_call_coverage())?;
        let end = usize::try_from(call.callee_range.end).map_err(|_| graph_call_coverage())?;
        let text = file
            .source()
            .get(start..end)
            .ok_or_else(graph_call_coverage)?;
        let offset = text
            .char_indices()
            .next_back()
            .map(|(offset, _)| start + offset)
            .ok_or_else(graph_call_coverage)?;
        let name = match call.callee {
            rift_syntax::StaticCallee::Member { member, .. } => file
                .source()
                .get(
                    usize::try_from(member.start).map_err(|_| graph_call_coverage())?
                        ..usize::try_from(member.end).map_err(|_| graph_call_coverage())?,
                )
                .ok_or_else(graph_call_coverage)?,
            _ => text.rsplit("::").next().unwrap_or(text),
        };
        sites.push(InventoryCallSite {
            name: name.to_owned(),
            at: offset as u64,
            caller,
            source: rift_protocol::read::SourceUnitSpan {
                unit: crate::read::source_unit_id(file.path()),
                range: rift_protocol::read::TextRange {
                    start: call.range.start,
                    end: call.range.end,
                },
            },
            utf8: lines.position(PositionEncoding::Utf8, offset)?,
            utf16: lines.position(PositionEncoding::Utf16, offset)?,
        });
    }
    Ok(sites)
}

async fn read_definitions(
    session: &mut EngineSession,
    path: &ProjectPath,
    sites: &[InventoryCallSite],
    callers: BTreeSet<SymbolId>,
    selection: EngineInventorySelection,
    report: &mut InventoryReport,
) -> Result<(), RiftError> {
    report.charge(callers.len())?;
    report.definition_callers = callers;
    let encoding = session.capabilities().position_encoding;
    let maximum = selection
        .batch_inputs
        .clamp(1, rift_lsp::correlation::PENDING_REQUESTS_MAX);
    for chunk in sites.chunks(maximum) {
        let positions: Vec<_> = chunk
            .iter()
            .map(|site| {
                (
                    path.clone(),
                    match encoding {
                        PositionEncoding::Utf8 => site.utf8,
                        PositionEncoding::Utf16 => site.utf16,
                    },
                )
            })
            .collect();
        let answers = session.definition_batch(&positions).await?;
        for (site, locations) in chunk.iter().zip(answers) {
            report.charge(locations.len().saturating_add(1))?;
            report.definitions.push(InventoryDefinition {
                name: site.name.clone(),
                at: site.at,
                caller: site.caller.clone(),
                source: site.source.clone(),
                locations,
            });
        }
    }
    Ok(())
}

fn resolve_definitions(
    reads: &ReadService,
    (packages, workspace_root): (&CalleeRoots, &std::path::Path),
    encoding: PositionEncoding,
    callers: BTreeSet<SymbolId>,
    definitions: Vec<InventoryDefinition>,
    provider: EdgeProvider,
    source_complete: bool,
) -> Result<Vec<(SymbolId, GraphCalls)>, RiftError> {
    let mut calls: BTreeMap<_, _> = callers
        .into_iter()
        .map(|caller| {
            (
                caller,
                GraphCalls {
                    complete: source_complete,
                    ..GraphCalls::default()
                },
            )
        })
        .collect();
    for definition in definitions {
        let answer = calls.entry(definition.caller.clone()).or_default();
        if definition.locations.is_empty() {
            answer.complete = false;
        }
        let caller = CoreSymbolId::new(definition.caller.as_str())?;
        for location in definition.locations {
            let file = super::callee_file(
                packages,
                &[workspace_root, reads.index().root()],
                &caller,
                super::NamedCallee {
                    provider,
                    uri: &location.uri,
                    name: &definition.name,
                    position: location.range.start,
                    encoding,
                },
            )?;
            match file {
                super::CalleeFile::Project(path) => {
                    if let Some(target) =
                        super::project_definition(reads, &path, (location.range, encoding), false)?
                    {
                        answer.push_local(
                            &definition.caller,
                            target,
                            vec![definition.source.clone()],
                            TraversalDirection::Outgoing,
                            provider,
                        );
                    } else {
                        answer.complete = false;
                    }
                }
                super::CalleeFile::Package(target) => {
                    answer
                        .packages
                        .push((target, vec![definition.source.clone()]));
                }
                super::CalleeFile::Unaddressed => answer.complete = false,
            }
        }
    }
    calls
        .into_iter()
        .map(|(seed, answer)| {
            let identity = CoreSymbolId::new(seed.as_str())?;
            let captured = CapturedCalls::read(reads, &identity, TraversalDirection::Outgoing);
            Ok((seed, captured.combine_external(answer)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::{SourceVisibility, TextFileInclusion};
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::configuration::HistoryConfiguration;

    #[test]
    fn captured_calls_still_read_the_engine_definition() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("graph.ts"),
            "function helper() {}\nexport function caller() { helper(); }\n",
        )?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let file = reads
            .index()
            .file(&ProjectPath::new("graph.ts")?)
            .ok_or("file")?;
        let (seeds, _) = reads.local_search_view_input(true)?.inventory_seeds()?;
        let caller = seeds
            .into_iter()
            .find(|seed| seed.as_str().ends_with("/caller"))
            .ok_or("caller")?;
        assert_eq!(
            inventory_call_sites(&reads, file, &BTreeSet::from([caller]))?.len(),
            1
        );
        Ok(())
    }

    #[test]
    fn call_definitions_select_the_local_binding_instead_of_its_helper()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let source =
            "function helper() {}\nconst local = helper;\nexport function caller() { local(); }\n";
        std::fs::write(directory.path().join("graph.ts"), source)?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let path = ProjectPath::new("graph.ts")?;
        let file = reads.index().file(&path).ok_or("file")?;
        let (seeds, _) = reads.local_search_view_input(true)?.inventory_seeds()?;
        let caller = seeds
            .iter()
            .find(|seed| seed.as_str().ends_with("/caller"))
            .ok_or("caller")?;
        let local = seeds
            .iter()
            .find(|seed| seed.as_str().ends_with("/local"))
            .ok_or("local")?;
        let sites = inventory_call_sites(&reads, file, &BTreeSet::from([caller.clone()]))?;
        assert_eq!(
            sites.len(),
            1,
            "a captured call still requires engine definition resolution"
        );
        let uri = rift_lsp::TreeRoot::new(directory.path())?.document_uri(&path)?;
        let calls = resolve_definitions(
            &reads,
            (&CalleeRoots::default(), directory.path()),
            PositionEncoding::Utf8,
            BTreeSet::from([caller.clone()]),
            vec![InventoryDefinition {
                name: sites[0].name.clone(),
                at: sites[0].at,
                caller: caller.clone(),
                source: sites[0].source.clone(),
                locations: vec![Location {
                    uri,
                    range: lsp_types::Range::new(Position::new(1, 6), Position::new(1, 11)),
                }],
            }],
            EdgeProvider::Lsp,
            true,
        )?;
        let [(_, answer)] = calls.as_slice() else {
            panic!("one caller")
        };
        assert_eq!(
            answer.local,
            vec![(local.clone(), vec![sites[0].source.clone()])]
        );
        Ok(())
    }

    #[test]
    fn call_definitions_keep_admitted_package_targets_complete()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let installed = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n[lib]\npath = 'lib.rs'\n[dependencies]\ndependency = '1'\n",
        )?;
        std::fs::write(
            directory.path().join("lib.rs"),
            "use dependency::alias;\npub fn caller() { alias(); }\n",
        )?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let path = ProjectPath::new("lib.rs")?;
        let file = reads.index().file(&path).ok_or("file")?;
        let (seeds, _) = reads.local_search_view_input(true)?.inventory_seeds()?;
        let caller = seeds
            .into_iter()
            .find(|seed| seed.as_str().ends_with("/caller"))
            .ok_or("caller")?;
        let sites = inventory_call_sites(&reads, file, &BTreeSet::from([caller.clone()]))?;
        let package = rift_core::PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: "dependency".to_owned(),
            version: "1.0.0".to_owned(),
        };
        let root = rift_lsp::TreeRoot::new(installed.path())?;
        let roots = CalleeRoots::from_packages(vec![rift_lsp::uri::PackageRoot::new(
            root.clone(),
            package,
        )]);
        let location = Location {
            uri: root.document_uri(&ProjectPath::new("src/lib.rs")?)?,
            range: lsp_types::Range::new(Position::new(2, 7), Position::new(2, 13)),
        };
        let calls = resolve_definitions(
            &reads,
            (&roots, directory.path()),
            PositionEncoding::Utf8,
            BTreeSet::from([caller.clone()]),
            vec![InventoryDefinition {
                name: sites[0].name.clone(),
                at: sites[0].at,
                caller,
                source: sites[0].source.clone(),
                locations: vec![location.clone()],
            }],
            EdgeProvider::Lsp,
            true,
        )?;
        let [(_, answer)] = calls.as_slice() else {
            panic!("one caller")
        };
        assert!(answer.complete);
        let spelled =
            SymbolId("rift://symbol/cargo/crates.io/dependency/rust/dependency/alias".to_owned());
        assert_eq!(
            answer.local,
            vec![(spelled.clone(), vec![sites[0].source.clone()])]
        );
        assert!(answer.corroboration.is_empty());
        assert!(
            answer
                .edges
                .iter()
                .any(|edge| edge.to == spelled && edge.provider == EdgeProvider::Static)
        );
        let [(target, ranges)] = answer.packages.as_slice() else {
            panic!("one admitted target")
        };
        assert_eq!(target.position(), location.range.start);
        assert_eq!(target.path().as_str(), "src/lib.rs");
        assert_eq!(ranges, &[sites[0].source.clone()]);
        assert_package_call_replaces_local(calls, &sites[0], &spelled);
        Ok(())
    }

    fn assert_package_call_replaces_local(
        calls: Vec<(SymbolId, GraphCalls)>,
        site: &InventoryCallSite,
        spelled: &SymbolId,
    ) {
        let seed = calls[0].0.clone();
        let merged = calls::merge_calls(
            vec![(
                seed.clone(),
                GraphCalls {
                    local: vec![(seed.clone(), vec![site.source.clone()])],
                    complete: true,
                    ..GraphCalls::default()
                },
            )],
            calls,
            vec![(seed, site.source.clone(), site.at)],
        );
        assert!(
            merged[0]
                .1
                .local
                .iter()
                .all(|(target, _)| target == spelled),
            "the dependency keeps its static spelling and replaces a conflicting local target"
        );
        assert_eq!(merged[0].1.packages.len(), 1);
        assert!(merged[0].1.complete);
    }

    #[test]
    fn implementation_answers_keep_only_direct_implements() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let source = "interface Parent {}\ninterface Child extends Parent {}\ninterface Grandchild extends Child {}\nclass Direct implements Parent {}\nclass Inherited extends Direct {}\n";
        std::fs::write(directory.path().join("graph.ts"), source)?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let (seeds, _) = reads.local_search_view_input(true)?.inventory_seeds()?;
        let parent = seeds
            .iter()
            .find(|seed| seed.as_str().ends_with("/Parent"))
            .ok_or("Parent")?;
        let direct = seeds
            .iter()
            .find(|seed| seed.as_str().ends_with("/Direct"))
            .ok_or("Direct")?;
        let path = ProjectPath::new("graph.ts")?;
        let uri = rift_lsp::TreeRoot::new(directory.path())?.document_uri(&path)?;
        let heritage = [(1, 15), (2, 20), (3, 12), (4, 15)]
            .into_iter()
            .map(|(line, end)| InventoryHeritage {
                seed: parent.clone(),
                kind: EdgeKind::Implements,
                direction: TraversalDirection::Incoming,
                location: Location {
                    uri: uri.clone(),
                    range: lsp_types::Range {
                        start: Position::new(line, if line < 3 { 10 } else { 6 }),
                        end: Position::new(line, end),
                    },
                },
            })
            .collect();
        let edges = resolve_heritage(
            &reads,
            directory.path(),
            PositionEncoding::Utf8,
            heritage,
            EdgeProvider::Lsp,
        )?;
        assert_eq!(
            edges.len(),
            1,
            "interfaces extend; a subclass does not add an indirect implements edge: {edges:?}"
        );
        assert_eq!(&edges[0].from, direct);
        assert_eq!(&edges[0].to, parent);
        Ok(())
    }

    /// rust-analyzer answers `textDocument/implementation` on a trait with the implementing
    /// type of each `impl` header, not with the trait the stored edge occurs at.
    #[test]
    fn implementation_answers_naming_the_implementing_type_confirm_the_stored_edge()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\npath = \"lib.rs\"\n",
        )?;
        let source = "pub trait Shape {\n    fn area(&self) -> f64;\n}\npub struct Square;\npub struct Circle;\nimpl Shape for Square {\n    fn area(&self) -> f64 {\n        1.0\n    }\n}\nimpl Shape for Circle {\n    fn area(&self) -> f64 {\n        3.0\n    }\n}\n";
        std::fs::write(directory.path().join("lib.rs"), source)?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let (seeds, _) = reads.local_search_view_input(true)?.inventory_seeds()?;
        let seed = |name: &str| {
            seeds
                .iter()
                .find(|seed| seed.as_str().ends_with(name))
                .cloned()
                .ok_or_else(|| format!("seed {name}"))
        };
        let shape = seed("/Shape")?;
        let path = ProjectPath::new("lib.rs")?;
        let uri = rift_lsp::TreeRoot::new(directory.path())?.document_uri(&path)?;
        // `impl Shape for Square {`: the implementing type starts at column 15.
        let heritage = [5, 10]
            .into_iter()
            .map(|line| InventoryHeritage {
                seed: shape.clone(),
                kind: EdgeKind::Implements,
                direction: TraversalDirection::Incoming,
                location: Location {
                    uri: uri.clone(),
                    range: lsp_types::Range {
                        start: Position::new(line, 15),
                        end: Position::new(line, 21),
                    },
                },
            })
            .collect();
        let edges = resolve_heritage(
            &reads,
            directory.path(),
            PositionEncoding::Utf8,
            heritage,
            EdgeProvider::Lsp,
        )?;
        let pairs: Vec<_> = edges
            .iter()
            .map(|edge| (edge.from.clone(), edge.to.clone(), edge.provider))
            .collect();
        assert_eq!(
            pairs,
            [
                (seed("/Square")?, shape.clone(), EdgeProvider::Lsp),
                (seed("/Circle")?, shape, EdgeProvider::Lsp),
            ],
            "each impl header confirms its own stored edge: {edges:?}"
        );
        Ok(())
    }
}
