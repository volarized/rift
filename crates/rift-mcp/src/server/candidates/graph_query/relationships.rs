//! Acquisition of complete adjacency from the selected providers.

use std::collections::{BTreeMap, BTreeSet};

use rift_cloud_client::relationships::RelationshipReadLimits;
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{
    PackageIdentity, ReadWarning, RelationshipFacet, RuntimeIdentity, Symbol, SymbolId,
    TraversalDirection,
};
use rift_protocol::relationship_read::{
    QueryOccurrence, RelationshipInclude, RelationshipSearchPage, RelationshipSearchRequest,
    RelationshipSearchResult, RelationshipSelection,
};
use rift_protocol::revision::Revision;
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use rift_search::sparql::{
    AdjacencySelection, AdjacencyStep, GraphDatasetBuilder, GraphFact, GraphLimits, RevisionIri,
    SymbolAddress, SymbolRelation,
};

use super::super::super::{RequestDeadline, ResolvedWorkspace, RiftMcp};
use crate::global::workspace_release;

const REQUESTS_MAX: u64 = 64;
const RECORDS_MAX: u64 = 10_000;

#[derive(Default)]
pub(super) struct AcquiredAdjacency {
    pages: Vec<(RelationshipSearchPage, SymbolRelation)>,
    local: Vec<QueryOccurrence>,
    local_edges: Vec<(SymbolId, SymbolRelation, SymbolId)>,
    named_package_symbols: Vec<(Symbol, RevisionIri)>,
    revisions: BTreeMap<SymbolOwner, RevisionIri>,
    view: Option<CapturedView>,
    warnings: Vec<ReadWarning>,
}

impl AcquiredAdjacency {
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
        for (source, relation, target) in &self.local_edges {
            dataset.insert(GraphFact::Relation {
                source,
                relation: *relation,
                target,
            })?;
        }
        for (symbol, revision) in &self.named_package_symbols {
            let id = symbol.id.as_ref().ok_or_else(coverage_failure)?;
            dataset.insert(GraphFact::SymbolDetails {
                address: SymbolAddress::Established(id),
                name: &symbol.name,
                kind: &symbol.kind.0,
                language: &symbol.language,
                revision,
            })?;
        }
        for occurrence in &self.local {
            if selected.usages {
                let range = &occurrence.source.range;
                let unit = &occurrence.source.unit;
                let key = format!(
                    "{} {} {} {}",
                    occurrence.target.as_str(),
                    unit.as_str(),
                    range.start,
                    range.end
                );
                let source = if selected.source {
                    Some(
                        input
                            .occurrence_source(&occurrence.source)
                            .ok_or_else(coverage_failure)?,
                    )
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
                for record in &group.relationships {
                    dataset.insert(GraphFact::Relation {
                        source: &record.relationship.from,
                        relation: *relation,
                        target: &record.relationship.to,
                    })?;
                }
                if selected.usages {
                    let sources: std::collections::BTreeMap<_, _> = page
                        .sources
                        .iter()
                        .map(|source| (&source.unit, source.text.as_deref()))
                        .collect();
                    for record in &group.occurrences {
                        let occurrence = &record.occurrence;
                        let source = if selected.source {
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
    fn admit_remote(
        &mut self,
        mut page: RelationshipSearchPage,
        step: AdjacencyStep,
        remaining: &mut RelationshipReadLimits,
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
                .relationships
                .iter()
                .map(|record| &record.id)
                .chain(group.occurrences.iter().map(|record| &record.id))
                .collect();
            charge_records(remaining, records.len())?;
            frontier.extend(
                group
                    .relationships
                    .iter()
                    .map(|record| record.relationship.to.clone()),
            );
        }
        self.view.get_or_insert_with(|| page.view.clone());
        for warning in page.warnings.drain(..) {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        self.pages.push((page, relation(step.facet)?));
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
        let mut walk = AdjacencyWalk {
            server: self,
            resolved,
            selected,
            step: selected.step(0),
            deadline,
            configuration: resolved.published.configuration.global_configuration(),
            remaining: RelationshipReadLimits {
                requests_max: REQUESTS_MAX,
                records_max: RECORDS_MAX,
                bytes_max: u64::try_from(limits.literal_bytes_max()).unwrap_or(u64::MAX),
            },
            acquired: AcquiredAdjacency::default(),
            remote: None,
            local: None,
            neighbors: BTreeMap::new(),
        };
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
                if SymbolIdentity::parse(seed.as_str())?.owner().is_local() {
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
        Ok(Ok(walk.acquired))
    }
}

struct AdjacencyWalk<'request> {
    server: &'request RiftMcp,
    resolved: &'request ResolvedWorkspace,
    selected: &'request AdjacencySelection,
    step: AdjacencyStep,
    deadline: RequestDeadline,
    configuration: rift_protocol::configuration::GlobalConfiguration,
    remaining: RelationshipReadLimits,
    acquired: AcquiredAdjacency,
    remote: Option<RemoteSelection>,
    local: Option<(
        std::sync::Arc<rift_server::EnginePool>,
        rift_server::CalleeRoots,
    )>,
    neighbors: BTreeMap<(SymbolId, AdjacencyStep), Vec<SymbolId>>,
}

impl AdjacencyWalk<'_> {
    async fn local_step(
        &mut self,
        seed: &SymbolId,
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        if self.resolved.stale.is_some() {
            return Err(coverage_failure());
        }
        claim_request(&mut self.remaining)?;
        let reads = &self.resolved.published.reads;
        let direction = self.step.direction;
        if self.local.is_none() {
            let engines = self.server.engine_pool_for(&self.resolved.published).await;
            let roots = if self.step.facet == RelationshipFacet::Calls
                || self.selected.has_distinct_steps()
            {
                let reads = std::sync::Arc::clone(reads);
                let engines = std::sync::Arc::clone(&engines);
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
        }
        let (engines, roots) = self.local.as_ref().ok_or_else(coverage_failure)?;
        let deadline = self.deadline.walk_end(self.resolved.capture_elapsed);
        match (direction, self.step.facet) {
            (TraversalDirection::Incoming, RelationshipFacet::References) => {
                let occurrences = Box::pin(rift_server::graph_references(
                    reads, engines, seed, deadline,
                ))
                .await?;
                charge_records(&mut self.remaining, occurrences.len())?;
                for occurrence in occurrences {
                    if let Some(caller) = &occurrence.enclosing {
                        self.acquired.local_edges.push((
                            caller.clone(),
                            SymbolRelation::References,
                            seed.clone(),
                        ));
                        frontier.insert(caller.clone());
                    }
                    self.acquired.local.push(occurrence);
                }
            }
            (_, RelationshipFacet::Calls) => {
                let mut calls = Box::pin(rift_server::graph_calls(
                    reads, engines, roots, seed, direction, deadline,
                ))
                .await?;
                self.name_package_calls(&mut calls).await?;
                charge_records(&mut self.remaining, calls.local.len())?;
                for (neighbor, ranges) in calls.local {
                    charge_records(&mut self.remaining, ranges.len())?;
                    let (caller, target) = match direction {
                        TraversalDirection::Outgoing => (seed, &neighbor),
                        TraversalDirection::Incoming => (&neighbor, seed),
                    };
                    self.acquired.local_edges.push((
                        caller.clone(),
                        SymbolRelation::Calls,
                        target.clone(),
                    ));
                    frontier.insert(neighbor.clone());
                    if self.selected.usages {
                        self.acquired.local.extend(ranges.into_iter().map(|source| {
                            QueryOccurrence {
                                target: target.clone(),
                                enclosing: Some(caller.clone()),
                                source,
                                facet: RelationshipFacet::Calls,
                            }
                        }));
                    }
                }
            }
            _ => return Err(coverage_failure()),
        }
        Ok(())
    }

    async fn name_package_calls(
        &mut self,
        calls: &mut rift_server::GraphCalls,
    ) -> Result<(), RiftError> {
        if calls.packages.is_empty() {
            return Ok(());
        }
        if !self.configuration.enabled {
            return Err(coverage_failure());
        }
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
            if workspace_release(dependencies, identity.owner()).as_ref()
                != Some(&declaration.revision)
            {
                return Err(coverage_failure());
            }
            if self.selected.symbol_details {
                let symbol = callee.symbol(&declaration).ok_or_else(coverage_failure)?;
                self.acquired.named_package_symbols.push((
                    symbol,
                    RevisionIri::parse(&declaration.revision.to_string())?,
                ));
            }
            calls.local.push((declaration.id, occurrences));
        }
        Ok(())
    }

    async fn remote_steps(
        &mut self,
        remote_seeds: &[SymbolId],
        frontier: &mut BTreeSet<SymbolId>,
    ) -> Result<Option<SearchUnavailable>, RiftError> {
        let selected = self.selected;
        let step = self.step;
        if step.direction != TraversalDirection::Outgoing || !self.configuration.enabled {
            return Err(coverage_failure());
        }
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
        let remote = self.remote.as_ref().ok_or_else(coverage_failure)?;
        let bounds = remote.client.relationship_bounds(
            &remote.capabilities,
            step.direction,
            &[step.facet],
        )?;
        let seeds_max = usize::try_from(bounds.max_seeds).unwrap_or(usize::MAX);
        for seeds in remote_seeds.chunks(seeds_max) {
            for seed in seeds {
                if !self
                    .acquired
                    .revisions
                    .contains_key(SymbolIdentity::parse(seed.as_str())?.owner())
                {
                    return Err(coverage_failure());
                }
            }
            let request = RelationshipSearchRequest {
                relationships: RelationshipSelection {
                    seeds: seeds.to_vec(),
                    direction: step.direction,
                    facets: vec![step.facet],
                },
                packages: remote.domain.packages.clone(),
                runtimes: remote.domain.runtimes.clone(),
                include: if selected.source {
                    vec![RelationshipInclude::Source]
                } else {
                    Vec::new()
                },
                view: self.acquired.view.as_ref().map(|view| view.id.clone()),
            };
            let page = remote
                .client
                .relationship_pages_with_admission(&request, self.remaining, || {
                    claim_request(&mut self.remaining)
                })
                .await?;
            let page = match page {
                RelationshipSearchResult::Page(page) => page,
                RelationshipSearchResult::Unavailable(reason) => return Ok(Some(reason)),
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
            if selected.has_distinct_steps() {
                for group in &page.groups {
                    let reached = group
                        .relationships
                        .iter()
                        .map(|record| record.relationship.to.clone())
                        .collect();
                    self.neighbors.insert((group.seed.clone(), step), reached);
                }
            }
            self.acquired
                .admit_remote(page, step, &mut self.remaining, frontier)?;
        }
        Ok(None)
    }
}

fn relation(facet: RelationshipFacet) -> Result<SymbolRelation, RiftError> {
    match facet {
        RelationshipFacet::Calls => Ok(SymbolRelation::Calls),
        RelationshipFacet::References => Ok(SymbolRelation::References),
        _ => Err(coverage_failure()),
    }
}

fn claim_request(limits: &mut RelationshipReadLimits) -> Result<(), RiftError> {
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
}

fn exact_domain(context: &rift_dependency::DependencyContext) -> Result<ExactDomain, RiftError> {
    let mut entries = BTreeMap::<SymbolOwner, Option<(String, Option<PackageIdentity>)>>::new();
    for entry in context.entries() {
        if entry.availability != rift_protocol::dependencies::PackageAvailability::Canonical {
            continue;
        }
        let Some(version) = &entry.version else {
            continue;
        };
        let (owner, package) = if entry.manager == "stdlib" {
            let owner = SymbolOwner::Runtime {
                runtime: entry.name.clone(),
            };
            (owner, None)
        } else {
            let Some(registry) = &entry.registry else {
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
    Ok(domain)
}

fn charge_records(limits: &mut RelationshipReadLimits, count: usize) -> Result<(), RiftError> {
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
