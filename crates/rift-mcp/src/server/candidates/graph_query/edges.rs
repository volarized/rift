//! Acquisition of complete adjacency from the selected providers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_cloud_client::edges::EdgeReadLimits;
use rift_error::{RiftError, errors};
use rift_protocol::edge_read::{
    EdgeInclude, EdgeSearchPage, EdgeSearchRequest, EdgeSearchResult, EdgeSelection,
    QueryOccurrence,
};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{
    EdgeKind, PackageIdentity, ReadWarning, RuntimeIdentity, Symbol, SymbolId, TraversalDirection,
};
use rift_protocol::revision::Revision;
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use rift_search::sparql::{
    AdjacencySelection, AdjacencyStep, GraphDatasetBuilder, GraphFact, GraphLimits,
    OccurrenceSelection, ReachedFacets, RevisionIri, SymbolAddress, SymbolRelation, SyntaxLocation,
};

use super::super::super::{RequestDeadline, ResolvedWorkspace, RiftMcp};
use super::exact::{exact_request, selected_release};
use super::installed::InstalledTarget;

mod flows;
pub(super) use flows::{AcquiredFlows, FlowAcquisition};

const REQUESTS_MAX: u64 = 64;
const RECORDS_MAX: u64 = 10_000;

#[derive(Default)]
pub(super) struct AcquiredAdjacency {
    pages: Vec<(EdgeSearchPage, SymbolRelation)>,
    local: Vec<QueryOccurrence>,
    local_edges: Vec<(SymbolId, SymbolRelation, SymbolId)>,
    named_package_symbols: BTreeMap<SymbolId, (Symbol, RevisionIri)>,
    installed: BTreeMap<SymbolId, Arc<InstalledTarget>>,
    revisions: BTreeMap<SymbolOwner, RevisionIri>,
    view: Option<CapturedView>,
    warnings: Vec<ReadWarning>,
    incomplete: bool,
    partial: bool,
}

/// The declarations a whole-inventory read walks and the facts it reads for each.
struct InventorySelection<'seeds> {
    seeds: &'seeds [SymbolId],
    calls: bool,
    references: bool,
    enclosing: bool,
}

/// Calls and references one whole-inventory read collected, and whether part of the
/// inventory is missing from them.
#[derive(Default)]
struct InventoryRead {
    local: Vec<QueryOccurrence>,
    local_edges: Vec<(SymbolId, SymbolRelation, SymbolId)>,
    warnings: Vec<ReadWarning>,
    partial: bool,
    records: usize,
    missing: MissingKinds,
}

/// The kinds some selected declaration's captured or engine answer left incomplete.
#[derive(Default)]
struct MissingKinds {
    calls: bool,
    references: bool,
}

impl InventoryRead {
    /// Marks the read partial and names the kinds whose answer is incomplete for some
    /// selected declaration.
    fn report_missing(&mut self) {
        let kinds: Vec<EdgeKind> = [
            (self.missing.calls, EdgeKind::Calls),
            (self.missing.references, EdgeKind::References),
        ]
        .into_iter()
        .filter_map(|(missing, kind)| missing.then_some(kind))
        .collect();
        if kinds.is_empty() {
            return;
        }
        self.partial = true;
        let named: Vec<&str> = kinds.iter().map(AsRef::as_ref).collect();
        self.extend_warnings([ReadWarning::EdgeCoverageMissing {
            detail: format!(
                "the captured facts do not cover every declaration the inventory selects \
                 for {}, so edges outside that coverage are missing from this answer",
                named.join(", ")
            ),
            kinds,
        }]);
    }

    fn charge(&mut self, records: usize) -> Result<(), RiftError> {
        let maximum = usize::try_from(RECORDS_MAX).unwrap_or(usize::MAX);
        self.records = self
            .records
            .checked_add(records)
            .filter(|records| *records <= maximum)
            .ok_or_else(coverage_failure)?;
        Ok(())
    }

    fn extend_warnings(&mut self, warnings: impl IntoIterator<Item = ReadWarning>) {
        for warning in warnings {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
    }

    fn admit_references(
        &mut self,
        seed: &SymbolId,
        occurrences: Vec<QueryOccurrence>,
    ) -> Result<(), RiftError> {
        self.charge(occurrences.len())?;
        for occurrence in occurrences {
            if let Some(caller) = &occurrence.enclosing {
                self.local_edges
                    .push((caller.clone(), SymbolRelation::References, seed.clone()));
            }
            self.local.push(occurrence);
        }
        Ok(())
    }

    fn admit_calls(
        &mut self,
        seed: &SymbolId,
        calls: Vec<(SymbolId, Vec<rift_protocol::read::SourceUnitSpan>)>,
    ) -> Result<(), RiftError> {
        for (target, occurrences) in calls {
            self.charge(1usize.saturating_add(occurrences.len()))?;
            self.local_edges
                .push((seed.clone(), SymbolRelation::Calls, target));
        }
        Ok(())
    }
}

/// Reads the inventory from the captured static facts alone.
fn captured_inventory(
    reads: &rift_server::ReadService,
    inventory: &InventorySelection<'_>,
) -> Result<InventoryRead, RiftError> {
    let mut read = InventoryRead::default();
    for seed in inventory.seeds {
        if inventory.references {
            let references =
                rift_server::captured_graph_references(reads, seed, inventory.enclosing)?;
            read.missing.references |= !references.complete;
            read.admit_references(seed, references.occurrences)?;
        }
        if inventory.calls {
            let calls =
                rift_server::captured_graph_calls(reads, seed, TraversalDirection::Outgoing)?;
            read.missing.calls |= !calls.complete;
            read.admit_calls(seed, calls.local)?;
        }
    }
    read.report_missing();
    Ok(read)
}

impl AcquiredAdjacency {
    fn admit_inventory(&mut self, read: InventoryRead) -> Result<(), RiftError> {
        let maximum = usize::try_from(RECORDS_MAX).unwrap_or(usize::MAX);
        if self.local_edges.len().saturating_add(read.records) > maximum {
            return Err(coverage_failure());
        }
        self.partial |= read.partial;
        self.local.extend(read.local);
        self.local_edges.extend(read.local_edges);
        for warning in read.warnings {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        Ok(())
    }

    /// Whether part of a whole inventory is missing while every acquired fact stands.
    pub(super) const fn is_partial(&self) -> bool {
        self.partial
    }

    fn capture_edge_inventory(
        &mut self,
        reads: &rift_server::ReadService,
    ) -> Result<(), RiftError> {
        self.incomplete |= !reads.edges().is_complete();
        let maximum = usize::try_from(RECORDS_MAX).map_err(|_| coverage_failure())?;
        for edge in reads.edges().edges() {
            if self.local_edges.len() >= maximum {
                return Err(coverage_failure());
            }
            self.local_edges.push((
                SymbolId(edge.from().as_str().to_owned()),
                relation(edge.kind())?,
                SymbolId(edge.to().as_str().to_owned()),
            ));
        }
        Ok(())
    }

    pub(super) const fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    pub(super) const fn remote_view(&self) -> Option<&CapturedView> {
        self.view.as_ref()
    }

    pub(super) fn take_warnings(&mut self) -> Vec<ReadWarning> {
        std::mem::take(&mut self.warnings)
    }

    pub(super) fn local_endpoints(&self) -> BTreeSet<&SymbolId> {
        self.local_edges
            .iter()
            .flat_map(|(from, _, to)| [from, to])
            .collect()
    }

    pub(super) fn project(
        &self,
        dataset: &mut GraphDatasetBuilder,
        selected: &AdjacencySelection,
        input: &rift_server::LocalSymbolViewInput,
    ) -> Result<(), RiftError> {
        self.project_local(dataset, input, true, selected.occurrences)?;
        for (id, (symbol, revision)) in &self.named_package_symbols {
            dataset.insert(GraphFact::SymbolDetails {
                address: SymbolAddress::Established(id),
                name: &symbol.name,
                kind: &symbol.kind.0,
                language: &symbol.language,
                revision,
            })?;
        }
        let mut captured = BTreeSet::new();
        if let Some(syntax_elements) = selected.syntax_elements {
            captured = self.project_captured_declarations(dataset, syntax_elements)?;
            for endpoint in self.local_endpoints() {
                if !SymbolIdentity::parse(endpoint.as_str())?.owner().is_local()
                    && !captured.contains(endpoint)
                    && !self.installed.contains_key(endpoint)
                {
                    return Err(coverage_failure());
                }
            }
        }
        for (id, target) in &self.installed {
            if !captured.contains(id) {
                target.project(
                    dataset,
                    selected
                        .syntax_elements
                        .map(|syntax_elements| syntax_elements.source),
                )?;
            }
        }
        self.project_remote(dataset, selected)
    }

    pub(super) fn project_local(
        &self,
        dataset: &mut GraphDatasetBuilder,
        input: &rift_server::LocalSymbolViewInput,
        edges: bool,
        occurrences: Option<OccurrenceSelection>,
    ) -> Result<(), RiftError> {
        if edges {
            for (source, relation, target) in &self.local_edges {
                dataset.insert(GraphFact::Relation {
                    source,
                    relation: *relation,
                    target,
                })?;
            }
        }
        if let Some(required) = occurrences {
            for occurrence in &self.local {
                let range = &occurrence.source.range;
                let unit = &occurrence.source.unit;
                let key = format!(
                    "{} {} {} {}",
                    occurrence.target.as_str(),
                    unit.as_str(),
                    range.start,
                    range.end
                );
                let source = if required.source {
                    let text = if let Some(installed) = occurrence
                        .enclosing
                        .as_ref()
                        .and_then(|id| self.installed.get(id))
                    {
                        if installed.unit != *unit {
                            return Err(coverage_failure());
                        }
                        source_slice(&installed.source, range)?
                    } else {
                        input
                            .occurrence_source(&occurrence.source)
                            .ok_or_else(coverage_failure)?
                    };
                    Some(text)
                } else {
                    None
                };
                dataset.insert(GraphFact::Occurrence {
                    key: &key,
                    occurrence,
                    source,
                })?;
            }
        }
        Ok(())
    }

    fn project_captured_declarations(
        &self,
        dataset: &mut GraphDatasetBuilder,
        syntax_elements: ReachedFacets,
    ) -> Result<BTreeSet<&SymbolId>, RiftError> {
        if syntax_elements.syntax_metadata && !self.pages.is_empty() {
            return Err(coverage_failure());
        }
        let mut symbols = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for (page, _) in &self.pages {
            for item in &page.symbols {
                let id = item.symbol.id.as_ref().ok_or_else(coverage_failure)?;
                if symbols
                    .insert(id, item)
                    .is_some_and(|previous| previous != item)
                {
                    return Err(coverage_failure());
                }
            }
            for item in &page.sources {
                if sources
                    .insert(&item.unit, item)
                    .is_some_and(|previous| previous != item)
                {
                    return Err(coverage_failure());
                }
            }
        }
        for (id, item) in &symbols {
            if let Some(installed) = self.installed.get(*id) {
                let source = sources
                    .get(&installed.unit)
                    .and_then(|source| source.text.as_deref())
                    .ok_or_else(coverage_failure)?;
                if source != installed.source.as_ref() {
                    return Err(coverage_failure());
                }
            }
            if item.declarations.is_empty()
                && item.symbol.origin.source_kind != rift_protocol::read::SourceKind::Synthetic
            {
                return Err(coverage_failure());
            }
            for (position, declaration) in item.declarations.iter().enumerate() {
                let unit = declaration.unit.as_ref().ok_or_else(coverage_failure)?;
                let text = sources
                    .get(unit)
                    .and_then(|source| source.text.as_deref())
                    .ok_or_else(coverage_failure)?;
                let excerpt = source_slice(text, &declaration.range)?;
                dataset.insert_declaration_excerpt(
                    id,
                    position,
                    SyntaxLocation {
                        unit,
                        range: &declaration.range,
                    },
                    syntax_elements.source.then_some(excerpt),
                )?;
            }
        }
        Ok(symbols.into_keys().collect())
    }

    fn project_remote(
        &self,
        dataset: &mut GraphDatasetBuilder,
        selected: &AdjacencySelection,
    ) -> Result<(), RiftError> {
        for (page, relation) in &self.pages {
            for item in &page.symbols {
                if let Some(symbol) = &item.symbol.id {
                    let identity = SymbolIdentity::parse(symbol.as_str())?;
                    let revision = self
                        .revisions
                        .get(identity.owner())
                        .ok_or_else(coverage_failure)?;
                    dataset.insert(GraphFact::SymbolDetails {
                        address: SymbolAddress::Established(symbol),
                        name: &item.symbol.name,
                        kind: &item.symbol.kind.0,
                        language: &item.symbol.language,
                        revision,
                    })?;
                }
            }
            for group in &page.groups {
                for record in &group.edges {
                    dataset.insert(GraphFact::Relation {
                        source: &record.edge.from,
                        relation: *relation,
                        target: &record.edge.to,
                    })?;
                }
                if let Some(usage) = selected.occurrences {
                    let sources: std::collections::BTreeMap<_, _> = page
                        .sources
                        .iter()
                        .map(|source| (&source.unit, source.text.as_deref()))
                        .collect();
                    for record in &group.occurrences {
                        let occurrence = &record.occurrence;
                        let source = if usage.source {
                            let text = sources
                                .get(&occurrence.source.unit)
                                .copied()
                                .flatten()
                                .ok_or_else(coverage_failure)?;
                            Some(source_slice(text, &occurrence.source.range)?)
                        } else {
                            None
                        };
                        dataset.insert(GraphFact::Occurrence {
                            key: record.id.as_str(),
                            occurrence,
                            source,
                        })?;
                    }
                }
            }
        }
        Ok(())
    }
    fn admit_references(&mut self, seed: &SymbolId, occurrences: Vec<QueryOccurrence>) {
        for occurrence in occurrences {
            if let Some(caller) = &occurrence.enclosing {
                self.local_edges
                    .push((caller.clone(), SymbolRelation::References, seed.clone()));
            }
            self.local.push(occurrence);
        }
    }

    fn admit_remote(
        &mut self,
        mut page: EdgeSearchPage,
        step: AdjacencyStep,
        remaining: &mut EdgeReadLimits,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let maximum = usize::try_from(remaining.bytes_max).unwrap_or(usize::MAX);
        let charged = crate::proxy::encode_bounded(&page, maximum)
            .map_err(super::encoding_failure)?
            .len();
        remaining.bytes_max = remaining
            .bytes_max
            .checked_sub(u64::try_from(charged).map_err(|_| coverage_failure())?)
            .ok_or_else(coverage_failure)?;
        for group in &page.groups {
            let records: BTreeSet<_> = group
                .edges
                .iter()
                .map(|record| &record.id)
                .chain(group.occurrences.iter().map(|record| &record.id))
                .collect();
            charge_records(remaining, records.len())?;
            frontier.extend(
                group
                    .edges
                    .iter()
                    .map(|record| endpoint(&record.edge, step.direction).clone()),
            );
        }
        self.view.get_or_insert_with(|| page.view.clone());
        for warning in page.warnings.drain(..) {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        self.pages.push((page, relation(step.kind)?));
        Ok(())
    }
}

impl RiftMcp {
    /// Acquires the calls, references and usages a whole-inventory query reads.
    ///
    /// The captured static facts answer first. A language engine adds what it resolves on
    /// top of them when every established declaration fits the engine requests one query
    /// may send and the index matches the captured tree; otherwise the inventory answers
    /// from the captured facts alone and an `engine_analysis_unavailable` warning names
    /// each language whose engine analysis is missing. Every condition that leaves part of
    /// the inventory out marks the acquisition partial.
    pub(super) async fn acquire_semantic_inventory(
        &self,
        resolved: &ResolvedWorkspace,
        required: rift_search::sparql::SemanticInventory,
        deadline: RequestDeadline,
    ) -> Result<AcquiredAdjacency, RiftError> {
        let reads = &resolved.published.reads;
        let mut acquired = AcquiredAdjacency::default();
        if required.edges {
            acquired.capture_edge_inventory(reads)?;
        }
        if !required.calls && !required.references && required.occurrences.is_none() {
            return Ok(acquired);
        }
        let references = required.references || required.occurrences.is_some();
        let enclosing =
            required.references || required.occurrences.is_some_and(|usage| usage.enclosing);
        let capture = Arc::clone(reads);
        let (seeds, coverage) = self
            .blocking
            .run("semantic inventory seeds", move || {
                capture.local_search_view_input(true)?.inventory_seeds()
            })
            .await?;
        acquired.partial |= coverage == rift_server::InventoryCoverage::Partial;
        let engines = self.engine_pool_for(&resolved.published).await;
        let mut served = BTreeMap::new();
        for seed in &seeds {
            let identity = SymbolIdentity::parse(seed.as_str())?;
            let language = identity.language();
            if engines.engine_for(language).is_some() {
                served
                    .entry(language.identity_segment())
                    .or_insert_with(|| language.clone());
            }
        }
        let inventory = InventorySelection {
            seeds: &seeds,
            calls: required.calls,
            references,
            enclosing,
        };
        let operations = u64::from(required.calls) + u64::from(references);
        let maximum = usize::try_from(REQUESTS_MAX / operations.max(1)).unwrap_or(usize::MAX);
        let skipped = if served.is_empty() {
            None
        } else if resolved.stale.is_some() {
            Some(
                "the published index lags the captured tree, so the inventory answers from \
                 the captured static facts alone"
                    .to_owned(),
            )
        } else if seeds.len() > maximum {
            Some(format!(
                "the inventory selects {} declarations, more than the {maximum} engine \
                 requests one query sends, so it answers from the captured static facts \
                 alone; select a seed declaration to read the engine's edges",
                seeds.len()
            ))
        } else {
            None
        };
        let engine = if served.is_empty() || skipped.is_some() {
            None
        } else {
            let end = deadline.walk_end(resolved.capture_elapsed);
            match Box::pin(self.engine_inventory(resolved, &engines, &inventory, end)).await {
                Ok(engine) => Some(engine),
                Err(error) if super::lacks_coverage(&error) => None,
                Err(error) => return Err(error),
            }
        };
        let read = if let Some(engine) = engine {
            engine
        } else {
            let detail = skipped.unwrap_or_else(|| {
                "the language engine could not answer for the whole inventory, so it answers \
                 from the captured static facts alone"
                    .to_owned()
            });
            let mut captured = captured_inventory(reads, &inventory)?;
            for language in served.into_values() {
                captured.partial = true;
                captured
                    .warnings
                    .push(ReadWarning::EngineAnalysisUnavailable {
                        language: Some(language),
                        detail: detail.clone(),
                    });
            }
            captured
        };
        acquired.admit_inventory(read)?;
        Ok(acquired)
    }

    /// Reads the inventory through the language engines, each seed's answer combined with
    /// the captured static facts. A seed whose engine answer is incomplete keeps what it
    /// answered and marks the read partial; callees in package files drop and are counted.
    async fn engine_inventory(
        &self,
        resolved: &ResolvedWorkspace,
        engines: &rift_server::EnginePool,
        inventory: &InventorySelection<'_>,
        end: tokio::time::Instant,
    ) -> Result<InventoryRead, RiftError> {
        let reads = &resolved.published.reads;
        let mut read = InventoryRead::default();
        let roots = rift_server::CalleeRoots::default();
        let mut dropped = 0u64;
        for seed in inventory.seeds {
            if inventory.references {
                let answered = Box::pin(rift_server::graph_references(
                    reads,
                    engines,
                    seed,
                    inventory.enclosing,
                    end,
                ))
                .await;
                let references = match answered {
                    Ok(references) => references,
                    Err(error) if super::lacks_coverage(&error) => {
                        rift_server::captured_graph_references(reads, seed, inventory.enclosing)?
                    }
                    Err(error) => return Err(error),
                };
                read.missing.references |= !references.complete;
                read.extend_warnings(references.edge_warnings);
                read.admit_references(seed, references.occurrences)?;
            }
            if inventory.calls {
                let answered = Box::pin(rift_server::graph_calls(
                    reads,
                    engines,
                    &roots,
                    seed,
                    TraversalDirection::Outgoing,
                    end,
                ))
                .await;
                let calls = match answered {
                    Ok(calls) => calls,
                    Err(error) if super::lacks_coverage(&error) => {
                        rift_server::captured_graph_calls(
                            reads,
                            seed,
                            TraversalDirection::Outgoing,
                        )?
                    }
                    Err(error) => return Err(error),
                };
                read.missing.calls |= !calls.complete;
                read.partial |= !calls.packages.is_empty();
                dropped =
                    dropped.saturating_add(u64::try_from(calls.packages.len()).unwrap_or(u64::MAX));
                read.extend_warnings(calls.edge_warnings);
                read.admit_calls(seed, calls.local)?;
            }
        }
        read.report_missing();
        if dropped > 0 {
            read.extend_warnings([ReadWarning::CalleesDropped {
                callees: dropped,
                detail: "the language engine named callees in package files, which a whole \
                         inventory carries no edge to; select a seed declaration to follow them"
                    .to_owned(),
            }]);
        }
        if !self
            .engine_tree_matches(&resolved.published)
            .await
            .map_err(|_| coverage_failure())?
        {
            return Err(coverage_failure());
        }
        Ok(read)
    }

    pub(super) async fn acquire_adjacency(
        &self,
        resolved: &ResolvedWorkspace,
        selected: &AdjacencySelection,
        limits: GraphLimits,
        deadline: RequestDeadline,
    ) -> Result<Result<AcquiredAdjacency, SearchUnavailable>, RiftError> {
        let mut walk = AdjacencyWalk::new(
            self,
            resolved,
            Some(selected),
            selected.step(0),
            limits,
            deadline,
        );
        let mut frontier = BTreeSet::from([selected.seed.clone()]);
        let mut visited = BTreeSet::new();
        let mut depth = 0u32;
        while !frontier.is_empty() && selected.depth.is_none_or(|maximum| depth < maximum) {
            walk.step = selected.step(depth);
            let mut remote_seeds = Vec::new();
            for seed in std::mem::take(&mut frontier) {
                let key = (seed.clone(), walk.step);
                if let Some(cached) = walk.neighbors.get(&key) {
                    frontier.extend(cached.iter().cloned());
                    continue;
                }
                if SymbolIdentity::parse(seed.as_str())?.owner().is_local()
                    || (walk.step.direction == TraversalDirection::Outgoing
                        && walk.acquired.installed.contains_key(&seed))
                {
                    let mut reached = BTreeSet::new();
                    walk.local_step(&seed, &mut reached).await?;
                    if selected.has_distinct_steps() {
                        walk.neighbors
                            .insert(key, reached.iter().cloned().collect());
                    }
                    frontier.extend(reached);
                    visited.insert(seed);
                } else {
                    visited.insert(seed.clone());
                    remote_seeds.push(seed);
                }
            }
            if !remote_seeds.is_empty()
                && let Some(unavailable) = walk.remote_steps(&remote_seeds, &mut frontier).await?
            {
                return Ok(Err(unavailable));
            }
            if !selected.has_distinct_steps() {
                frontier.retain(|seed| !visited.contains(seed));
            }
            depth = depth.checked_add(1).ok_or_else(coverage_failure)?;
        }
        if walk.local.is_some()
            && !self
                .engine_tree_matches(&resolved.published)
                .await
                .map_err(|_| coverage_failure())?
        {
            return Err(coverage_failure());
        }
        walk.verify_installed().await?;
        Ok(Ok(walk.acquired))
    }
}

struct AdjacencyWalk<'request> {
    server: &'request RiftMcp,
    resolved: &'request ResolvedWorkspace,
    selected: Option<&'request AdjacencySelection>,
    step: AdjacencyStep,
    deadline: RequestDeadline,
    configuration: rift_protocol::configuration::GlobalConfiguration,
    remaining: EdgeReadLimits,
    acquired: AcquiredAdjacency,
    remote: Option<RemoteSelection>,
    local: Option<(
        std::sync::Arc<rift_server::EnginePool>,
        rift_server::CalleeRoots,
    )>,
    neighbors: BTreeMap<(SymbolId, AdjacencyStep), Vec<SymbolId>>,
    local_reverse: Option<BTreeMap<SymbolId, BTreeSet<SymbolId>>>,
    use_installed: Option<bool>,
}

impl<'request> AdjacencyWalk<'request> {
    fn new(
        server: &'request RiftMcp,
        resolved: &'request ResolvedWorkspace,
        selected: Option<&'request AdjacencySelection>,
        step: AdjacencyStep,
        limits: GraphLimits,
        deadline: RequestDeadline,
    ) -> Self {
        Self {
            server,
            resolved,
            selected,
            step,
            deadline,
            configuration: resolved.published.configuration.global_configuration(),
            remaining: EdgeReadLimits {
                requests_max: REQUESTS_MAX,
                records_max: RECORDS_MAX,
                bytes_max: u64::try_from(limits.literal_bytes_max()).unwrap_or(u64::MAX),
            },
            acquired: AcquiredAdjacency::default(),
            remote: None,
            local: None,
            neighbors: BTreeMap::new(),
            local_reverse: None,
            use_installed: None,
        }
    }

    async fn ensure_local(&mut self) -> Result<(), RiftError> {
        if self.resolved.stale.is_some() {
            return Err(coverage_failure());
        }
        if self.local.is_some() {
            return Ok(());
        }
        let engines = self.server.engine_pool_for(&self.resolved.published).await;
        let roots = if self.step.kind == EdgeKind::Calls
            || self
                .selected
                .is_some_and(AdjacencySelection::has_distinct_steps)
        {
            let reads = Arc::clone(&self.resolved.published.reads);
            let engines = Arc::clone(&engines);
            self.server
                .blocking
                .run("graph callee package roots", move || {
                    Ok(rift_server::CalleeRoots::read(&reads, &engines))
                })
                .await?
        } else {
            rift_server::CalleeRoots::default()
        };
        self.local = Some((engines, roots));
        Ok(())
    }

    async fn local_step(
        &mut self,
        seed: &SymbolId,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        self.ensure_local().await?;
        claim_request(&mut self.remaining)?;
        let reads = &self.resolved.published.reads;
        let direction = self.step.direction;
        let (engines, _) = self.local.as_ref().ok_or_else(coverage_failure)?;
        let deadline = self.deadline.walk_end(self.resolved.capture_elapsed);
        match (direction, self.step.kind) {
            (TraversalDirection::Incoming, EdgeKind::References) => {
                let references = Box::pin(rift_server::graph_references(
                    reads,
                    engines,
                    seed,
                    self.selected
                        .is_some_and(AdjacencySelection::requires_enclosing),
                    deadline,
                ))
                .await;
                let references = match references {
                    Ok(references) => references,
                    Err(error)
                        if error.slug() == errors::server::search_coverage_unavailable::SLUG =>
                    {
                        self.acquired.incomplete = true;
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                self.acquired.incomplete |= !references.complete;
                self.acquired.warnings.extend(references.edge_warnings);
                let occurrences = references.occurrences;
                charge_records(&mut self.remaining, occurrences.len())?;
                frontier.extend(occurrences.iter().filter_map(|item| item.enclosing.clone()));
                self.acquired.admit_references(seed, occurrences);
            }
            (TraversalDirection::Outgoing, EdgeKind::References) => {
                let references = Box::pin(rift_server::outgoing_reference_edges(
                    reads, engines, seed, deadline,
                ))
                .await?;
                self.acquired.incomplete |= !references.complete;
                self.acquired.warnings.extend(references.edge_warnings);
                charge_records(&mut self.remaining, references.occurrences.len())?;
                for occurrence in references.occurrences {
                    frontier.insert(occurrence.target.clone());
                    self.acquired.local_edges.push((
                        seed.clone(),
                        SymbolRelation::References,
                        occurrence.target.clone(),
                    ));
                    self.acquired.local.push(occurrence);
                }
            }
            (_, EdgeKind::Calls) => self.edge_call_step(seed, frontier).await?,
            (_, kind @ (EdgeKind::Extends | EdgeKind::Implements)) => {
                let (edges, warnings) = Box::pin(rift_server::graph_heritage_edges(
                    reads, engines, seed, kind, direction, deadline,
                ))
                .await?;
                self.edge_static_step(seed, kind, frontier)?;
                self.acquired.incomplete |= warnings.iter().any(|warning| {
                    matches!(warning, ReadWarning::EngineAnalysisUnavailable { .. })
                });
                self.acquired.warnings.extend(warnings);
                charge_records(&mut self.remaining, edges.len())?;
                for edge in edges {
                    let neighbor = match direction {
                        TraversalDirection::Outgoing => edge.to.clone(),
                        TraversalDirection::Incoming => edge.from.clone(),
                    };
                    frontier.insert(neighbor);
                    self.acquired
                        .local_edges
                        .push((edge.from, relation(kind)?, edge.to));
                }
            }
            (_, kind) => self.edge_static_step(seed, kind, frontier)?,
        }
        Ok(())
    }

    async fn edge_call_step(
        &mut self,
        seed: &SymbolId,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let reads = &self.resolved.published.reads;
        let (engines, roots) = self.local.as_ref().ok_or_else(coverage_failure)?;
        let direction = self.step.direction;
        let deadline = self.deadline.walk_end(self.resolved.capture_elapsed);
        let answer = if let Some(target) = self.acquired.installed.get(seed) {
            if direction != TraversalDirection::Outgoing {
                return Err(coverage_failure());
            }
            Box::pin(rift_server::installed_graph_calls(
                reads,
                engines,
                roots,
                seed,
                (
                    &target.callee,
                    &target.symbol.language,
                    &target.unit,
                    &target.source,
                ),
                deadline,
            ))
            .await
        } else {
            Box::pin(rift_server::graph_calls(
                reads, engines, roots, seed, direction, deadline,
            ))
            .await
        };
        let mut calls = match answer {
            Ok(calls) => calls,
            Err(error) if error.slug() == errors::server::search_coverage_unavailable::SLUG => {
                self.acquired.incomplete = true;
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        self.acquired.incomplete |= !calls.complete;
        self.acquired.warnings.append(&mut calls.edge_warnings);
        let details = self
            .selected
            .is_some_and(|selection| selection.symbol_details);
        if let Err(error) = self.name_package_calls(&mut calls, details).await {
            if self.selected.is_some() || !super::lacks_coverage(&error) {
                return Err(error);
            }
            self.acquired.incomplete = true;
        }
        self.admit_local_calls(seed, calls, frontier)?;
        Ok(())
    }

    fn edge_static_step(
        &mut self,
        seed: &SymbolId,
        kind: EdgeKind,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let store = self.resolved.published.reads.edges();
        self.acquired.incomplete |= !store.is_complete();
        let identity = rift_core::SymbolId::new(seed.as_str())?;
        let adjacent = match self.step.direction {
            TraversalDirection::Incoming => store.incoming(&identity),
            TraversalDirection::Outgoing => store.outgoing(&identity),
        };
        let relation = relation(kind)?;
        for edge in adjacent.iter().filter(|edge| edge.kind() == kind) {
            charge_records(&mut self.remaining, 1)?;
            let from = SymbolId(edge.from().as_str().to_owned());
            let to = SymbolId(edge.to().as_str().to_owned());
            frontier.insert(match self.step.direction {
                TraversalDirection::Incoming => from.clone(),
                TraversalDirection::Outgoing => to.clone(),
            });
            self.acquired.local_edges.push((from, relation, to));
        }
        Ok(())
    }

    fn admit_local_calls(
        &mut self,
        seed: &SymbolId,
        calls: rift_server::GraphCalls,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        charge_records(&mut self.remaining, calls.corroboration.len())?;
        for conflict in calls.corroboration.iter().filter(|record| !record.agrees) {
            let engine_target = match &conflict.engine_target {
                rift_server::CallDestination::Local(target) => target.as_str(),
                rift_server::CallDestination::Dependency(target) => target.path().as_str(),
            };
            rift_tracing::warn!(
                component = "edges",
                authored = ?conflict.authored,
                observed = ?conflict.observed,
                static_target = %conflict.static_target.as_str(),
                engine_target,
                provider = ?conflict.provider,
                "providers resolved the same call to different targets"
            );
        }
        if self.selected.is_none() {
            self.acquired.local.extend(
                calls
                    .corroboration
                    .iter()
                    .filter(|record| record.agrees)
                    .map(|record| {
                        let (caller, target) = match self.step.direction {
                            TraversalDirection::Outgoing => (seed, &record.static_target),
                            TraversalDirection::Incoming => (&record.static_target, seed),
                        };
                        QueryOccurrence {
                            target: target.clone(),
                            enclosing: Some(caller.clone()),
                            source: record.observed.clone(),
                            kind: EdgeKind::Calls,
                        }
                    }),
            );
        }
        charge_records(&mut self.remaining, calls.local.len())?;
        for (neighbor, ranges) in calls.local {
            charge_records(&mut self.remaining, ranges.len())?;
            let (caller, target) = match self.step.direction {
                TraversalDirection::Outgoing => (seed, &neighbor),
                TraversalDirection::Incoming => (&neighbor, seed),
            };
            self.acquired
                .local_edges
                .push((caller.clone(), SymbolRelation::Calls, target.clone()));
            frontier.insert(neighbor.clone());
            if self
                .selected
                .is_none_or(|selection| selection.occurrences.is_some())
            {
                self.acquired
                    .local
                    .extend(ranges.into_iter().map(|source| QueryOccurrence {
                        target: target.clone(),
                        enclosing: Some(caller.clone()),
                        source,
                        kind: EdgeKind::Calls,
                    }));
            }
        }
        Ok(())
    }

    async fn name_package_calls(
        &mut self,
        calls: &mut rift_server::GraphCalls,
        lookup_details: bool,
    ) -> Result<(), RiftError> {
        if calls.packages.is_empty() {
            return Ok(());
        }
        let installed = if let Some(installed) = self.use_installed {
            installed
        } else {
            let installed = if self.configuration.enabled {
                let client = self.server.global.client(&self.configuration).await?;
                let capabilities = client.get_capabilities().await?;
                let operations = rift_cloud_client::GlobalClient::edge_capabilities(&capabilities)?;
                !operations.iter().any(|operation| {
                    operation.direction == TraversalDirection::Outgoing
                        && operation.kinds.contains(&EdgeKind::Calls)
                        && operation.bounds.is_some()
                })
            } else {
                true
            };
            self.use_installed = Some(installed);
            installed
        };
        if installed {
            return self.capture_installed_calls(calls).await;
        }
        self.name_published_calls(calls, lookup_details).await
    }

    async fn name_published_calls(
        &mut self,
        calls: &mut rift_server::GraphCalls,
        lookup_details: bool,
    ) -> Result<(), RiftError> {
        claim_request(&mut self.remaining)?;
        let callees: Vec<_> = calls
            .packages
            .iter()
            .map(|(callee, _)| callee.clone())
            .collect();
        let client = self.server.global.client(&self.configuration).await?;
        let declarations = crate::global::callee_declarations(&client, &callees, &[]).await?;
        let dependencies = self.resolved.published.reads.dependency_context();
        for (callee, occurrences) in std::mem::take(&mut calls.packages) {
            let declaration = declarations
                .declaration(&callee)
                .ok_or_else(coverage_failure)?;
            let identity = SymbolIdentity::parse(declaration.id.as_str())?;
            if selected_release(dependencies, identity.owner()).as_ref()
                != Some(&declaration.revision)
            {
                return Err(coverage_failure());
            }
            if lookup_details
                && !self
                    .acquired
                    .named_package_symbols
                    .contains_key(&declaration.id)
            {
                claim_request(&mut self.remaining)?;
                let answer = client
                    .get_symbol(&exact_request(&declaration.id, &declaration.revision))
                    .await?;
                let rift_protocol::symbol_read::GetSymbolResult::Found {
                    symbol, revision, ..
                } = answer
                else {
                    return Err(coverage_failure());
                };
                if revision != declaration.revision {
                    return Err(coverage_failure());
                }
                self.acquired.named_package_symbols.insert(
                    declaration.id.clone(),
                    (*symbol, RevisionIri::parse(&revision.to_string())?),
                );
            }
            calls.local.push((declaration.id, occurrences));
        }
        Ok(())
    }

    async fn capture_installed_calls(
        &mut self,
        calls: &mut rift_server::GraphCalls,
    ) -> Result<(), RiftError> {
        let (_, roots) = self.local.as_ref().ok_or_else(coverage_failure)?;
        let roots = roots.clone();
        let tree = self.server.root.clone();
        let configuration = self
            .resolved
            .published
            .configuration
            .accepted_for_tool(super::super::super::wire::ErrorPhase::Read)
            .map_err(|_| coverage_failure())?;
        let packages = std::mem::take(&mut calls.packages);
        let maximum = usize::try_from(self.remaining.bytes_max).unwrap_or(usize::MAX);
        let targets = self
            .server
            .blocking
            .run("capture installed dependency targets", move || {
                let mut retained = 0usize;
                let mut targets = Vec::new();
                for (callee, occurrences) in packages {
                    let target = InstalledTarget::capture(callee, &roots, &tree, &configuration)?;
                    retained = retained
                        .checked_add(target.retained_bytes())
                        .filter(|size| *size <= maximum)
                        .ok_or_else(coverage_failure)?;
                    targets.push((target, occurrences));
                }
                Ok(targets)
            })
            .await?;
        for (target, occurrences) in targets {
            let identity = target.id()?.clone();
            let owner = SymbolIdentity::parse(identity.as_str())?;
            if selected_release(
                self.resolved.published.reads.dependency_context(),
                owner.owner(),
            )
            .as_ref()
                != Some(&target.revision)
            {
                return Err(coverage_failure());
            }
            self.remaining.bytes_max = self
                .remaining
                .bytes_max
                .checked_sub(
                    u64::try_from(target.retained_bytes()).map_err(|_| coverage_failure())?,
                )
                .ok_or_else(coverage_failure)?;
            if let Some(held) = self.acquired.installed.get(&identity)
                && !held.same_capture(&target)
            {
                return Err(coverage_failure());
            }
            self.acquired
                .installed
                .insert(identity.clone(), Arc::new(target));
            calls.local.push((identity, occurrences));
        }
        Ok(())
    }

    async fn verify_installed(&self) -> Result<(), RiftError> {
        if self.acquired.installed.is_empty() {
            return Ok(());
        }
        let (_, roots) = self.local.as_ref().ok_or_else(coverage_failure)?;
        let roots = roots.clone();
        let tree = self.server.root.clone();
        let targets: Vec<_> = self.acquired.installed.values().cloned().collect();
        self.server
            .blocking
            .run("verify installed dependency sources", move || {
                for target in targets {
                    target.verify(&roots, &tree)?;
                }
                Ok(())
            })
            .await
    }

    async fn acquire_local_reverse(&mut self) -> Result<(), RiftError> {
        if self.local_reverse.is_some() {
            return Ok(());
        }
        self.ensure_local().await?;
        let reads = &self.resolved.published.reads;
        let capture = Arc::clone(reads);
        let maximum = usize::try_from(self.remaining.requests_max).unwrap_or(usize::MAX);
        let seeds = self
            .server
            .blocking
            .run("local reverse caller seeds", move || {
                capture
                    .local_search_view_input(true)?
                    .semantic_seeds(maximum)
            })
            .await?;
        let published_declarations = self.remote.as_ref().is_some_and(|remote| {
            remote
                .capabilities
                .supported_features
                .iter()
                .any(|feature| feature == "declarations")
        });
        let deadline = self.deadline.walk_end(self.resolved.capture_elapsed);
        let mut callers = BTreeMap::<SymbolId, BTreeSet<SymbolId>>::new();
        for seed in seeds {
            claim_request(&mut self.remaining)?;
            let (engines, roots) = self.local.as_ref().ok_or_else(coverage_failure)?;
            let mut calls = Box::pin(rift_server::graph_calls(
                reads,
                engines,
                roots,
                &seed,
                TraversalDirection::Outgoing,
                deadline,
            ))
            .await?;
            if !calls.complete {
                return Err(coverage_failure());
            }
            if !calls.packages.is_empty() {
                if published_declarations {
                    self.name_published_calls(&mut calls, false).await?;
                } else {
                    self.capture_installed_calls(&mut calls).await?;
                }
            }
            charge_records(&mut self.remaining, calls.local.len())?;
            for (target, occurrences) in calls.local {
                charge_records(&mut self.remaining, occurrences.len())?;
                let owner = SymbolIdentity::parse(target.as_str())?;
                if !owner.owner().is_local() {
                    callers.entry(target).or_default().insert(seed.clone());
                }
            }
        }
        self.local_reverse = Some(callers);
        Ok(())
    }

    fn admit_local_reverse(
        &mut self,
        seeds: &[SymbolId],
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let captured = self.local_reverse.as_ref().ok_or_else(coverage_failure)?;
        for seed in seeds {
            if let Some(callers) = captured.get(seed) {
                for caller in callers {
                    self.acquired.local_edges.push((
                        caller.clone(),
                        SymbolRelation::Calls,
                        seed.clone(),
                    ));
                    frontier.insert(caller.clone());
                }
            }
        }
        Ok(())
    }

    async fn ensure_remote(&mut self) -> Result<(), RiftError> {
        if self.remote.is_none() {
            let reads = &self.resolved.published.reads;
            let mut domain = exact_domain(reads.dependency_context())?;
            if domain.packages.is_empty() && domain.runtimes.is_empty() {
                return Err(coverage_failure());
            }
            let client = self.server.global.client(&self.configuration).await?;
            let capabilities = client.get_capabilities().await?;
            self.acquired.revisions = std::mem::take(&mut domain.revisions);
            self.remote = Some(RemoteSelection {
                client,
                domain,
                capabilities,
            });
        }
        Ok(())
    }

    fn remember_remote_neighbors(&mut self, step: AdjacencyStep) -> Result<(), RiftError> {
        let incoming = step.direction == TraversalDirection::Incoming;
        let (page, _) = self.acquired.pages.last().ok_or_else(coverage_failure)?;
        for group in &page.groups {
            let local = self
                .local_reverse
                .as_ref()
                .and_then(|callers| callers.get(&group.seed))
                .into_iter()
                .flatten()
                .filter(|_| incoming);
            let reached = group
                .edges
                .iter()
                .map(|record| endpoint(&record.edge, step.direction))
                .chain(local)
                .cloned()
                .collect();
            self.neighbors.insert((group.seed.clone(), step), reached);
        }
        Ok(())
    }

    async fn remote_steps(
        &mut self,
        remote_seeds: &[SymbolId],
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<Option<SearchUnavailable>, RiftError> {
        let selected = self.selected.ok_or_else(coverage_failure)?;
        let step = self.step;
        let incoming = step.direction == TraversalDirection::Incoming;
        if !self.configuration.enabled
            || (incoming && step.kind != EdgeKind::Calls)
            || selected
                .syntax_elements
                .is_some_and(|syntax_elements| syntax_elements.syntax_metadata)
        {
            return Err(coverage_failure());
        }
        self.ensure_remote().await?;
        let remote = self.remote.as_ref().ok_or_else(coverage_failure)?;
        if incoming && !remote.domain.complete {
            return Err(coverage_failure());
        }
        let bounds =
            remote
                .client
                .edge_bounds(&remote.capabilities, step.direction, &[step.kind])?;
        let needs_source = selected.syntax_elements.is_some()
            || selected.occurrences.is_some_and(|usage| usage.source);
        let include = EdgeInclude {
            source: needs_source,
        };
        let seeds_max = usize::try_from(bounds.max_seeds).unwrap_or(usize::MAX);
        for seeds in remote_seeds.chunks(seeds_max) {
            let remote = self.remote.as_ref().ok_or_else(coverage_failure)?;
            for seed in seeds {
                if !self
                    .acquired
                    .revisions
                    .contains_key(SymbolIdentity::parse(seed.as_str())?.owner())
                {
                    return Err(coverage_failure());
                }
            }
            let request = EdgeSearchRequest {
                edges: EdgeSelection {
                    seeds: seeds.to_vec(),
                    direction: step.direction,
                    kinds: vec![step.kind],
                },
                packages: remote.domain.packages.clone(),
                runtimes: remote.domain.runtimes.clone(),
                include,
                view: self.acquired.view.as_ref().map(|view| view.id.clone()),
            };
            let page = remote
                .client
                .edge_pages_with_admission(&request, self.remaining, || {
                    claim_request(&mut self.remaining)
                })
                .await?;
            let page = match page {
                EdgeSearchResult::Page(page) => page,
                EdgeSearchResult::Unavailable(reason) => return Ok(Some(reason)),
            };
            if self
                .acquired
                .view
                .as_ref()
                .is_some_and(|view| view != &page.view)
                || self.acquired.pages.first().is_some_and(|(first, _)| {
                    first.analyzer_revision != page.analyzer_revision
                        || first.corpus_revision != page.corpus_revision
                })
            {
                return Ok(Some(super::unavailable(
                    SymbolUnavailableReason::ViewContext,
                )));
            }
            self.acquired
                .admit_remote(page, step, &mut self.remaining, frontier)?;
            if incoming {
                Box::pin(self.acquire_local_reverse()).await?;
                self.admit_local_reverse(seeds, frontier)?;
            }
            if selected.has_distinct_steps() {
                self.remember_remote_neighbors(step)?;
            }
        }
        Ok(None)
    }
}

fn relation(kind: EdgeKind) -> Result<SymbolRelation, RiftError> {
    match kind {
        EdgeKind::Calls => Ok(SymbolRelation::Calls),
        EdgeKind::References => Ok(SymbolRelation::References),
        EdgeKind::Imports => Ok(SymbolRelation::Imports),
        EdgeKind::Aliases => Ok(SymbolRelation::Aliases),
        EdgeKind::Extends => Ok(SymbolRelation::Extends),
        EdgeKind::Implements => Ok(SymbolRelation::Implements),
        EdgeKind::Declares => Ok(SymbolRelation::Declares),
        EdgeKind::Reads => Ok(SymbolRelation::Reads),
        EdgeKind::Writes => Ok(SymbolRelation::Writes),
        EdgeKind::HasType => Ok(SymbolRelation::HasType),
        _ => Err(coverage_failure()),
    }
}

fn endpoint(edge: &rift_protocol::read::Edge, direction: TraversalDirection) -> &SymbolId {
    match direction {
        TraversalDirection::Outgoing => &edge.to,
        TraversalDirection::Incoming => &edge.from,
    }
}

fn claim_request(limits: &mut EdgeReadLimits) -> Result<(), RiftError> {
    limits.requests_max = limits
        .requests_max
        .checked_sub(1)
        .ok_or_else(coverage_failure)?;
    Ok(())
}

struct RemoteSelection {
    client: rift_cloud_client::GlobalClient,
    domain: ExactDomain,
    capabilities: rift_cloud_client::Capabilities,
}

#[derive(Default)]
struct ExactDomain {
    packages: Vec<PackageIdentity>,
    runtimes: Vec<RuntimeIdentity>,
    revisions: BTreeMap<SymbolOwner, RevisionIri>,
    complete: bool,
}

fn exact_domain(context: &rift_dependency::DependencyContext) -> Result<ExactDomain, RiftError> {
    let mut entries = BTreeMap::<SymbolOwner, Option<(String, Option<PackageIdentity>)>>::new();
    let mut complete = !context.is_degraded();
    for entry in context.entries() {
        if entry.availability != rift_protocol::dependencies::PackageAvailability::Canonical {
            complete = false;
            continue;
        }
        let Some(version) = &entry.version else {
            complete = false;
            continue;
        };
        let (owner, package) = if entry.manager == rift_dependency::STANDARD_LIBRARY_MANAGER {
            let Some(library) = rift_dependency::StandardLibrary::from_name(&entry.name) else {
                complete = false;
                continue;
            };
            let owner = SymbolOwner::Runtime {
                runtime: library.runtime().to_owned(),
            };
            (owner, None)
        } else {
            let Some(registry) = &entry.registry else {
                complete = false;
                continue;
            };
            let package = PackageIdentity {
                manager: entry.manager.clone(),
                registry: registry.clone(),
                name: entry.name.clone(),
                version: version.clone(),
            };
            (package.owner()?, Some(package))
        };
        match entries.entry(owner) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(Some((version.clone(), package)));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if slot
                    .get()
                    .as_ref()
                    .is_some_and(|(previous, _)| previous != version)
                {
                    slot.insert(None);
                }
            }
        }
    }
    let mut domain = ExactDomain::default();
    for (owner, selection) in entries {
        let Some((version, package)) = selection else {
            complete = false;
            continue;
        };
        let revision = Revision::release(&owner, &version)?;
        if let Some(package) = package {
            domain.packages.push(package);
        } else if let SymbolOwner::Runtime { runtime } = &owner {
            domain.runtimes.push(RuntimeIdentity {
                runtime: runtime.clone(),
                version,
            });
        }
        domain
            .revisions
            .insert(owner, RevisionIri::parse(&revision.to_string())?);
    }
    domain.complete = complete;
    Ok(domain)
}

fn charge_records(limits: &mut EdgeReadLimits, count: usize) -> Result<(), RiftError> {
    limits.records_max = limits
        .records_max
        .checked_sub(u64::try_from(count).map_err(|_| coverage_failure())?)
        .ok_or_else(coverage_failure)?;
    Ok(())
}

fn source_slice<'source>(
    text: &'source str,
    range: &rift_protocol::read::TextRange,
) -> Result<&'source str, RiftError> {
    let start = usize::try_from(range.start).map_err(|_| coverage_failure())?;
    let end = usize::try_from(range.end).map_err(|_| coverage_failure())?;
    text.get(start..end).ok_or_else(coverage_failure)
}

pub(super) fn coverage_failure() -> RiftError {
    errors::server::search_coverage_unavailable()
        .operation("graph query adjacency")
        .error()
}
