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
use super::installed::{InstalledAnalyses, InstalledTarget};

mod flows;
mod inventory;
mod targets;
pub(super) use flows::{AcquiredFlows, FlowAcquisition};
pub(crate) use inventory::EngineInventoryCache;
use inventory::InventoryRead;

const REQUESTS_MAX: u64 = 64;
const RECORDS_MAX: u64 = 10_000;

#[derive(Default)]
pub(super) struct AcquiredAdjacency {
    pages: Vec<(EdgeSearchPage, SymbolRelation)>,
    local: Vec<QueryOccurrence>,
    local_edges: Vec<(SymbolId, SymbolRelation, SymbolId)>,
    edge_facts: Vec<rift_protocol::index::StoredEdge>,
    captured_inventory: Option<rift_search::sparql::SemanticInventory>,
    named_package_symbols: BTreeMap<SymbolId, (Symbol, RevisionIri)>,
    installed: BTreeMap<SymbolId, Arc<InstalledTarget>>,
    revisions: BTreeMap<SymbolOwner, RevisionIri>,
    view: Option<CapturedView>,
    warnings: Vec<ReadWarning>,
    incomplete: bool,
    partial: bool,
}

impl AcquiredAdjacency {
    fn admit_inventory(&mut self, read: &InventoryRead) -> Result<(), RiftError> {
        let maximum = GraphLimits::default().triples_max();
        if self.local_edges.len().saturating_add(read.records) > maximum {
            return Err(coverage_failure());
        }
        self.admit_package_details(read)?;
        self.partial |= read.partial;
        self.edge_facts.extend(read.edge_facts.iter().cloned());
        self.local.extend(read.local.iter().cloned());
        self.local_edges.extend(read.local_edges.iter().cloned());
        for (seed, calls) in &read.calls {
            self.edge_facts.extend(calls.edges.iter().cloned());
            for (target, ranges) in &calls.local {
                self.local
                    .extend(ranges.iter().cloned().map(|source| QueryOccurrence {
                        target: target.clone(),
                        enclosing: Some(seed.clone()),
                        source,
                        kind: EdgeKind::Calls,
                    }));
            }
            self.local_edges.extend(
                calls
                    .local
                    .iter()
                    .map(|(target, _)| (seed.clone(), SymbolRelation::Calls, target.clone())),
            );
        }
        for warning in &read.warnings {
            if !self.warnings.contains(warning) {
                self.warnings.push(warning.clone());
            }
        }
        Ok(())
    }

    fn admit_package_details(&mut self, read: &InventoryRead) -> Result<(), RiftError> {
        for (identity, target) in &read.installed {
            if let Some(held) = self.installed.get(identity)
                && !held.same_capture(target)
            {
                return Err(coverage_failure());
            }
            self.installed.insert(identity.clone(), Arc::clone(target));
        }
        self.named_package_symbols.extend(
            read.named_package_symbols
                .iter()
                .map(|(id, symbol)| (id.clone(), symbol.clone())),
        );
        Ok(())
    }

    /// Whether part of a whole inventory is missing while every acquired fact stands.
    pub(super) const fn is_partial(&self) -> bool {
        self.partial
    }

    fn capture_edge_inventory(
        &mut self,
        reads: &rift_server::ReadService,
        required: rift_search::sparql::SemanticInventory,
    ) -> Result<(), RiftError> {
        self.incomplete |= !reads.static_edges_complete()?;
        let omitted = reads.static_targets_omitted();
        if !omitted.is_empty() {
            let sites = omitted.values().copied().fold(0, u64::saturating_add);
            let warning = ReadWarning::EdgeCoverageMissing {
                kinds: omitted.keys().copied().collect(),
                detail: format!(
                    "static analysis left {sites} sites without a target, so edges of these \
                     kinds may be missing from this answer"
                ),
            };
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        // Captures without identities are left out of the inventory: its rows stand, but an
        // absence or an aggregate over it is no claim the inventory can make.
        self.partial |= !reads.edge_identities_complete();
        self.captured_inventory = Some(required);
        Ok(())
    }

    pub(super) fn project_captured_inventory(
        &self,
        dataset: &mut GraphDatasetBuilder,
        input: &rift_server::LocalSymbolViewInput,
        reads: &rift_server::ReadService,
        cancellation: &rift_search::sparql::QueryCancellation,
        deadline: RequestDeadline,
    ) -> Result<(), RiftError> {
        let Some(required) = self.captured_inventory else {
            return Ok(());
        };
        // Captured edges already belong to the bounded publication. Project them
        // directly so the dataset charges distinct triples and literal bytes.
        for edge in reads.edges().edges() {
            if cancellation.is_cancelled() {
                return errors::search::query_cancelled().fail();
            }
            if std::time::Instant::now() >= deadline.at().into_std() {
                return Err(super::deadline_failure(deadline.budget()));
            }
            if required.edges.metadata {
                dataset.insert_edge(&edge.to_protocol().into(), None)?;
            }
            let source = SymbolId(edge.from().as_str().to_owned());
            let target = SymbolId(edge.to().as_str().to_owned());
            if required.edges.relations || required.adjacency.calls || required.adjacency.references
            {
                dataset.insert(GraphFact::Relation {
                    source: &source,
                    relation: relation(edge.kind())?,
                    target: &target,
                })?;
            }
            let usage = (required.adjacency.references || required.occurrences.is_some())
                && matches!(
                    edge.kind(),
                    EdgeKind::Calls
                        | EdgeKind::References
                        | EdgeKind::Imports
                        | EdgeKind::Aliases
                        | EdgeKind::Extends
                        | EdgeKind::Implements
                        | EdgeKind::Reads
                        | EdgeKind::Writes
                        | EdgeKind::HasType
                );
            if !usage {
                continue;
            }
            if required.adjacency.references {
                dataset.insert(GraphFact::Relation {
                    source: &source,
                    relation: SymbolRelation::References,
                    target: &target,
                })?;
            }
            if let Some(selection) = required.occurrences {
                self.project_occurrence(dataset, input, &captured_occurrence(edge), selection)?;
            }
        }
        Ok(())
    }

    pub(super) const fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    /// Whether the read is incomplete or left part of an inventory out: either way an
    /// absence or an aggregate over it is no claim the acquired facts make.
    pub(super) const fn leaves_facts_out(&self) -> bool {
        self.incomplete || self.partial
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
        self.project_local(
            dataset,
            input,
            rift_search::sparql::SemanticEdges {
                relations: true,
                metadata: true,
            },
            selected.occurrences,
        )?;
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
        edges: rift_search::sparql::SemanticEdges,
        occurrences: Option<OccurrenceSelection>,
    ) -> Result<(), RiftError> {
        if edges.metadata {
            for edge in &self.edge_facts {
                dataset.insert_edge(&edge.clone().into(), None)?;
            }
        }
        if edges.relations {
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
                self.project_occurrence(dataset, input, occurrence, required)?;
            }
        }
        Ok(())
    }

    fn project_occurrence(
        &self,
        dataset: &mut GraphDatasetBuilder,
        input: &rift_server::LocalSymbolViewInput,
        occurrence: &QueryOccurrence,
        required: OccurrenceSelection,
    ) -> Result<(), RiftError> {
        let range = &occurrence.source.range;
        let unit = &occurrence.source.unit;
        // Equal locations can carry records with different kinds or enclosing symbols.
        let key = format!(
            "{} {} {} {} {} {}",
            occurrence.target.as_str(),
            unit.as_str(),
            range.start,
            range.end,
            occurrence.kind.as_ref(),
            occurrence.enclosing.as_ref().map_or("", SymbolId::as_str)
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
                    dataset.insert_edge(&record.edge, Some(&record.id))?;
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
        reached: &mut Option<AcquisitionBudget>,
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
            charge_records(remaining, reached, records.len())?;
            frontier.extend(group.edges.iter().map(|record| {
                endpoint((&record.edge.from, &record.edge.to), step.direction).clone()
            }));
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
        'walk: while !frontier.is_empty() && selected.depth.is_none_or(|maximum| depth < maximum) {
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
                    let step = walk.local_step(&seed, &mut reached).await;
                    if walk.spent(step)? {
                        break 'walk;
                    }
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
            if !remote_seeds.is_empty() {
                let remote = walk.remote_steps(&remote_seeds, &mut frontier).await;
                if walk.reached.is_some() {
                    break 'walk;
                }
                if let Some(unavailable) = remote? {
                    return Ok(Err(unavailable));
                }
            }
            if !selected.has_distinct_steps() {
                frontier.retain(|seed| !visited.contains(seed));
            }
            depth = depth.checked_add(1).ok_or_else(coverage_failure)?;
        }
        if let Some(budget) = walk.reached {
            budget.record();
        }
        if walk.local.is_some()
            && (!walk.captured_readiness_only || walk.reached.is_some())
            && !self
                .engine_tree_matches(&resolved.published)
                .await
                .map_err(|_| coverage_failure())?
        {
            return Err(coverage_failure());
        }
        if (walk.local.is_some() || walk.local_reverse.is_some())
            && let Some(warning) = resolved.published.reads.edge_identity_warning()
        {
            walk.acquired.warnings.push(warning);
            walk.acquired.partial = true;
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
    /// The acquisition budget the walk spent, once one is: the walk then stops and the
    /// acquired facts stand.
    reached: Option<AcquisitionBudget>,
    acquired: AcquiredAdjacency,
    remote: Option<RemoteSelection>,
    local: Option<(
        std::sync::Arc<rift_server::EnginePool>,
        rift_server::CalleeRoots,
    )>,
    neighbors: BTreeMap<(SymbolId, AdjacencyStep), Vec<SymbolId>>,
    local_reverse: Option<BTreeMap<SymbolId, BTreeSet<SymbolId>>>,
    /// Each local step has proved it used only its accepted capture after a readiness refusal.
    captured_readiness_only: bool,
    use_installed: Option<bool>,
    /// Package analyses this walk's installed targets were placed in, one per source set.
    installed_analyses: InstalledAnalyses,
}

impl<'request> AdjacencyWalk<'request> {
    /// Whether `step` ended because the walk spent an acquisition budget, which stops the
    /// walk with the facts it acquired; any other failure of the step is returned.
    fn spent(&self, step: Result<(), RiftError>) -> Result<bool, RiftError> {
        match step {
            Ok(()) => Ok(false),
            Err(_) if self.reached.is_some() => Ok(true),
            Err(error) => Err(error),
        }
    }

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
            reached: None,
            acquired: AcquiredAdjacency::default(),
            remote: None,
            local: None,
            neighbors: BTreeMap::new(),
            local_reverse: None,
            captured_readiness_only: true,
            use_installed: None,
            installed_analyses: InstalledAnalyses::default(),
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
        let reads = Arc::clone(&self.resolved.published.reads);
        let capture = Arc::clone(&engines);
        let roots = self
            .server
            .blocking
            .run("graph package roots", move || {
                Ok(rift_server::CalleeRoots::read(&reads, &capture))
            })
            .await?;
        self.local = Some((engines, roots));
        Ok(())
    }

    async fn local_step(
        &mut self,
        seed: &SymbolId,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        self.ensure_local().await?;
        claim_request(&mut self.remaining, &mut self.reached)?;
        self.captured_readiness_only &= self.step.kind == EdgeKind::Calls;
        let direction = self.step.direction;
        match (direction, self.step.kind) {
            (TraversalDirection::Incoming, EdgeKind::References) => {
                self.incoming_references(seed, frontier).await?;
            }
            (TraversalDirection::Outgoing, EdgeKind::References) => {
                self.outgoing_references(seed, frontier).await?;
            }
            (_, EdgeKind::HasType) => self.type_step(seed, frontier).await?,
            (_, EdgeKind::Calls) => self.edge_call_step(seed, frontier).await?,
            (_, kind @ (EdgeKind::Extends | EdgeKind::Implements)) => {
                self.heritage_step(seed, kind, frontier).await?;
            }
            (_, kind) => self.edge_static_step(seed, kind, frontier)?,
        }
        Ok(())
    }

    async fn heritage_step(
        &mut self,
        seed: &SymbolId,
        kind: EdgeKind,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let direction = self.step.direction;
        let selected = (direction == TraversalDirection::Outgoing).then_some(seed);
        let inventory = Box::pin(self.server.inventory_for_kind(
            self.resolved,
            kind,
            selected,
            None,
            self.deadline.graph_walk_end(self.resolved.capture_elapsed),
        ))
        .await?;
        self.acquired.admit_package_details(&inventory)?;
        self.acquired.incomplete |= inventory.partial;
        self.acquired
            .warnings
            .extend(inventory.warnings.iter().cloned());
        self.acquired.edge_facts.extend(
            inventory
                .edge_facts
                .iter()
                .filter(|edge| {
                    edge.kinds.contains(&kind)
                        && match direction {
                            TraversalDirection::Outgoing => &edge.from == seed,
                            TraversalDirection::Incoming => &edge.to == seed,
                        }
                })
                .cloned(),
        );
        self.edge_static_step(seed, kind, frontier)?;
        for (from, relation, to) in inventory.edges(seed, relation(kind)?, direction) {
            charge_records(&mut self.remaining, &mut self.reached, 1)?;
            frontier.insert(match direction {
                TraversalDirection::Outgoing => to.clone(),
                TraversalDirection::Incoming => from.clone(),
            });
            self.acquired
                .local_edges
                .push((from.clone(), *relation, to.clone()));
        }
        Ok(())
    }

    async fn type_step(
        &mut self,
        seed: &SymbolId,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let direction = self.step.direction;
        let selected = (direction == TraversalDirection::Outgoing).then_some(seed);
        let deadline = self.deadline.graph_walk_end(self.resolved.capture_elapsed);
        let inventory = Box::pin(self.server.inventory_for_kind(
            self.resolved,
            EdgeKind::HasType,
            selected,
            None,
            deadline,
        ))
        .await?;
        self.acquired.admit_package_details(&inventory)?;
        self.acquired.incomplete |= inventory.partial;
        self.acquired
            .warnings
            .extend(inventory.warnings.iter().cloned());
        self.edge_static_step(seed, EdgeKind::HasType, frontier)?;
        self.acquired.edge_facts.extend(
            inventory
                .edge_facts
                .iter()
                .filter(|edge| {
                    edge.kinds.contains(&EdgeKind::HasType)
                        && endpoint((&edge.from, &edge.to), direction) != seed
                        && match direction {
                            TraversalDirection::Outgoing => &edge.from == seed,
                            TraversalDirection::Incoming => &edge.to == seed,
                        }
                })
                .cloned(),
        );
        for (from, kind, to) in inventory.edges(seed, SymbolRelation::HasType, direction) {
            charge_records(&mut self.remaining, &mut self.reached, 1)?;
            frontier.insert(match direction {
                TraversalDirection::Outgoing => to.clone(),
                TraversalDirection::Incoming => from.clone(),
            });
            self.acquired
                .local_edges
                .push((from.clone(), *kind, to.clone()));
        }
        Ok(())
    }

    async fn incoming_references(
        &mut self,
        seed: &SymbolId,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let reads = &self.resolved.published.reads;
        let (engines, _) = self.local.as_ref().ok_or_else(coverage_failure)?;
        let deadline = self.deadline.graph_walk_end(self.resolved.capture_elapsed);
        let references = Box::pin(rift_server::graph_references(
            reads,
            engines,
            seed,
            self.selected
                .is_some_and(AdjacencySelection::requires_enclosing),
            rift_server::EngineWalkDeadline::AdmitBefore(deadline),
        ))
        .await;
        let references = match references {
            Ok(references) => references,
            Err(error) if error.slug() == errors::server::search_coverage_unavailable::SLUG => {
                self.acquired.incomplete = true;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        self.acquired.incomplete |= !references.complete;
        self.acquired.edge_facts.extend(references.edges);
        self.acquired.warnings.extend(references.edge_warnings);
        let occurrences = references.occurrences;
        charge_records(&mut self.remaining, &mut self.reached, occurrences.len())?;
        frontier.extend(occurrences.iter().filter_map(|item| item.enclosing.clone()));
        self.acquired.admit_references(seed, occurrences);
        Ok(())
    }

    async fn outgoing_references(
        &mut self,
        seed: &SymbolId,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let deadline = self.deadline.graph_walk_end(self.resolved.capture_elapsed);
        let inventory = Box::pin(self.server.inventory_for_kind(
            self.resolved,
            EdgeKind::References,
            None,
            self.selected.map_or(
                Some(rift_search::sparql::OccurrenceSelection {
                    source: false,
                    enclosing: true,
                }),
                |selected| selected.occurrences,
            ),
            deadline,
        ))
        .await?;
        self.acquired.admit_package_details(&inventory)?;
        self.acquired.incomplete |= inventory.partial;
        self.acquired
            .warnings
            .extend(inventory.warnings.iter().cloned());
        self.acquired.edge_facts.extend(
            inventory
                .edge_facts
                .iter()
                .filter(|edge| edge.from == *seed && edge.kinds.contains(&EdgeKind::References))
                .cloned(),
        );
        if let Some(indices) = inventory.outgoing_references.get(seed) {
            charge_records(&mut self.remaining, &mut self.reached, indices.len())?;
            for index in indices {
                let occurrence = inventory.local[*index].clone();
                frontier.insert(occurrence.target.clone());
                self.acquired.local_edges.push((
                    seed.clone(),
                    SymbolRelation::References,
                    occurrence.target.clone(),
                ));
                self.acquired.local.push(occurrence);
            }
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
        let deadline = self.deadline.graph_walk_end(self.resolved.capture_elapsed);
        let answer = if let Some(target) = self.acquired.installed.get(seed) {
            self.captured_readiness_only = false;
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
        } else if direction == TraversalDirection::Outgoing
            && !self.configuration.enabled
            && self.selected.is_some()
        {
            let inventory = Box::pin(self.server.inventory_for_kind(
                self.resolved,
                EdgeKind::Calls,
                Some(seed),
                None,
                deadline,
            ))
            .await?;
            self.captured_readiness_only &= inventory.captured_readiness_only;
            self.acquired
                .warnings
                .extend(inventory.warnings.iter().cloned());
            Ok(inventory.calls.get(seed).cloned().unwrap_or_default())
        } else {
            self.captured_readiness_only = false;
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
            if !super::lacks_coverage(&error) {
                return Err(error);
            }
            // The unnamed dependency does not erase independently established
            // local calls. Only a positive ASK witness can use incomplete adjacency.
            self.acquired.incomplete = true;
            calls.packages.clear();
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
            charge_records(&mut self.remaining, &mut self.reached, 1)?;
            let from = SymbolId(edge.from().as_str().to_owned());
            let to = SymbolId(edge.to().as_str().to_owned());
            frontier.insert(match self.step.direction {
                TraversalDirection::Incoming => from.clone(),
                TraversalDirection::Outgoing => to.clone(),
            });
            self.acquired.local_edges.push((from, relation, to));
            self.acquired.edge_facts.push(edge.to_protocol());
        }
        Ok(())
    }

    fn admit_local_calls(
        &mut self,
        seed: &SymbolId,
        calls: rift_server::GraphCalls,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        charge_records(
            &mut self.remaining,
            &mut self.reached,
            calls.corroboration.len(),
        )?;
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
        self.acquired.edge_facts.extend(calls.edges);
        charge_records(&mut self.remaining, &mut self.reached, calls.local.len())?;
        for (neighbor, ranges) in calls.local {
            charge_records(&mut self.remaining, &mut self.reached, ranges.len())?;
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
        self.captured_readiness_only = false;
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
        let deadline = self.deadline.graph_walk_end(self.resolved.capture_elapsed);
        let mut callers = BTreeMap::<SymbolId, BTreeSet<SymbolId>>::new();
        for seed in seeds {
            claim_request(&mut self.remaining, &mut self.reached)?;
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
                self.name_package_calls(&mut calls, false).await?;
            }
            charge_records(&mut self.remaining, &mut self.reached, calls.local.len())?;
            self.acquired.edge_facts.extend(calls.edges);
            for (target, occurrences) in calls.local {
                charge_records(&mut self.remaining, &mut self.reached, occurrences.len())?;
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
                .map(|record| endpoint((&record.edge.from, &record.edge.to), step.direction))
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
            providers: true,
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
                    claim_request(&mut self.remaining, &mut self.reached)
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
            self.acquired.admit_remote(
                page,
                step,
                &mut self.remaining,
                &mut self.reached,
                frontier,
            )?;
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

fn captured_occurrence(edge: &rift_index::Edge) -> QueryOccurrence {
    let binding = edge.occurrence();
    QueryOccurrence {
        target: SymbolId(edge.to().as_str().to_owned()),
        enclosing: Some(SymbolId(edge.from().as_str().to_owned())),
        source: rift_protocol::read::SourceUnitSpan {
            unit: rift_protocol::read::SourceUnitId(binding.unit().to_string()),
            range: rift_protocol::read::TextRange {
                start: binding.range().start(),
                end: binding.range().end(),
            },
        },
        kind: edge.kind(),
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
        EdgeKind::Contains => Ok(SymbolRelation::Contains),
        EdgeKind::Overrides => Ok(SymbolRelation::Overrides),
        EdgeKind::Declares => Ok(SymbolRelation::Declares),
        EdgeKind::Reads => Ok(SymbolRelation::Reads),
        EdgeKind::Writes => Ok(SymbolRelation::Writes),
        EdgeKind::HasType => Ok(SymbolRelation::HasType),
        _ => Err(coverage_failure()),
    }
}

fn endpoint<'edge>(
    (from, to): (&'edge SymbolId, &'edge SymbolId),
    direction: TraversalDirection,
) -> &'edge SymbolId {
    match direction {
        TraversalDirection::Outgoing => to,
        TraversalDirection::Incoming => from,
    }
}

/// An acquisition budget one graph walk spent before it reached every edge it selects.
///
/// Reaching one is a steady state of the walk, not a missing fact: the walk stops, the facts
/// it acquired stand, the answer is evaluated and paged over them, and the walk writes the
/// budget to the log once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AcquisitionBudget {
    /// The walk claimed [`REQUESTS_MAX`] edge reads.
    Requests,
    /// The walk admitted [`RECORDS_MAX`] edge records.
    Records,
}

impl AcquisitionBudget {
    /// The name and value of the budget, as the log record carries them.
    const fn bound(self) -> (&'static str, u64) {
        match self {
            Self::Requests => ("requests_max", REQUESTS_MAX),
            Self::Records => ("records_max", RECORDS_MAX),
        }
    }

    /// Writes the one record naming the budget the walk spent.
    fn record(self) {
        let (bound, value) = self.bound();
        rift_tracing::warn!(
            component = "search",
            operation = "search.graph.acquire",
            bound = bound,
            value = value,
            "a graph walk reached its acquisition budget; the answer holds the acquired facts"
        );
    }
}

/// Claims one edge read, or records that the walk spent its request budget.
///
/// The error only unwinds the step that asked; the walk reads `reached` to tell a spent
/// budget, which stops it with every fact it acquired, from a missing fact, which refuses.
fn claim_request(
    limits: &mut EdgeReadLimits,
    reached: &mut Option<AcquisitionBudget>,
) -> Result<(), RiftError> {
    let Some(left) = limits.requests_max.checked_sub(1) else {
        *reached = Some(AcquisitionBudget::Requests);
        return Err(coverage_failure());
    };
    limits.requests_max = left;
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
                runtime: if library == rift_dependency::StandardLibrary::Rust {
                    "rust"
                } else {
                    library.runtime()
                }
                .to_owned(),
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

/// Charges `count` edge records, or records that the walk spent its record budget, as
/// [`claim_request`] does for reads.
fn charge_records(
    limits: &mut EdgeReadLimits,
    reached: &mut Option<AcquisitionBudget>,
    count: usize,
) -> Result<(), RiftError> {
    let left = u64::try_from(count)
        .ok()
        .and_then(|count| limits.records_max.checked_sub(count));
    let Some(left) = left else {
        *reached = Some(AcquisitionBudget::Records);
        return Err(coverage_failure());
    };
    limits.records_max = left;
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

#[cfg(test)]
mod budget_tests {
    use super::{
        AcquisitionBudget, EdgeReadLimits, RECORDS_MAX, REQUESTS_MAX, charge_records, claim_request,
    };

    fn budget() -> EdgeReadLimits {
        EdgeReadLimits {
            requests_max: REQUESTS_MAX,
            records_max: RECORDS_MAX,
            bytes_max: u64::MAX,
        }
    }

    /// D37 (#715): the request budget at its edges. Every claim up to `REQUESTS_MAX` is
    /// admitted; the one past it records the spent budget for the walk to stop on, rather
    /// than leaving a refusal the walk cannot tell from a missing fact.
    #[test]
    fn reaching_request_budget_keeps_acquisition_available() {
        let mut limits = budget();
        let mut reached = None;
        for claimed in 1..=REQUESTS_MAX {
            assert!(
                claim_request(&mut limits, &mut reached).is_ok(),
                "claim {claimed} of {REQUESTS_MAX}"
            );
            assert_eq!(reached, None, "claim {claimed}");
        }
        assert_eq!(limits.requests_max, 0);
        assert!(claim_request(&mut limits, &mut reached).is_err());
        assert_eq!(reached, Some(AcquisitionBudget::Requests));
        assert_eq!(limits.requests_max, 0, "a refused claim spends nothing");
    }

    /// D37 (#715): the record budget at its edges: below and at `RECORDS_MAX` the records are
    /// admitted, one past it records the spent budget.
    #[test]
    fn reaching_record_budget_records_the_spent_budget() {
        let records = usize::try_from(RECORDS_MAX).expect("fits");
        for (charged, admitted) in [(records - 1, true), (records, true), (records + 1, false)] {
            let mut limits = budget();
            let mut reached = None;
            assert_eq!(
                charge_records(&mut limits, &mut reached, charged).is_ok(),
                admitted,
                "charged={charged}"
            );
            assert_eq!(
                reached,
                (!admitted).then_some(AcquisitionBudget::Records),
                "charged={charged}"
            );
        }
    }

    /// The walk writes one warning record naming the budget it spent and its value.
    #[test]
    fn a_spent_budget_writes_one_record_with_its_value() {
        let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .capture("warn")
            .install()
            .expect("recorder");
        AcquisitionBudget::Requests.record();
        AcquisitionBudget::Records.record();
        let records: Vec<(String, u64)> = drain
            .queued_records()
            .into_iter()
            .filter(|record| {
                record
                    .message()
                    .starts_with("a graph walk reached its acquisition budget")
            })
            .map(|record| {
                assert_eq!(record.level(), "warn", "{record:?}");
                let fields: serde_json::Value =
                    serde_json::from_str(record.fields()).expect("fields");
                (
                    fields["bound"].as_str().expect("bound").to_owned(),
                    fields["value"]
                        .as_u64()
                        .or_else(|| fields["value"].as_str().and_then(|text| text.parse().ok()))
                        .expect("value"),
                )
            })
            .collect();
        assert_eq!(
            records,
            [
                ("requests_max".to_owned(), REQUESTS_MAX),
                ("records_max".to_owned(), RECORDS_MAX)
            ]
        );
    }
}
