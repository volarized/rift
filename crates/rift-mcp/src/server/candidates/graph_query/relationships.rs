//! Acquisition of complete adjacency from the selected providers.

use std::collections::{BTreeMap, BTreeSet};

use rift_cloud_client::relationships::RelationshipReadLimits;
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{
    PackageIdentity, ReadWarning, RelationshipFacet, RuntimeIdentity, SymbolId, TraversalDirection,
};
use rift_protocol::relationship_read::{
    QueryOccurrence, RelationshipInclude, RelationshipSearchPage, RelationshipSearchRequest,
    RelationshipSearchResult, RelationshipSelection,
};
use rift_protocol::revision::Revision;
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use rift_search::sparql::{
    AdjacencySelection, GraphDatasetBuilder, GraphFact, GraphLimits, RevisionIri, SymbolAddress,
    SymbolRelation,
};

use super::super::super::{RequestDeadline, ResolvedWorkspace, RiftMcp};
use crate::global::workspace_release;

const REQUESTS_MAX: u64 = 64;
const RECORDS_MAX: u64 = 10_000;

#[derive(Default)]
pub(super) struct AcquiredAdjacency {
    pages: Vec<RelationshipSearchPage>,
    local: Vec<QueryOccurrence>,
    local_edges: Vec<(SymbolId, SymbolId)>,
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

    pub(super) fn project(
        &self,
        dataset: &mut GraphDatasetBuilder,
        selected: &AdjacencySelection,
        input: &rift_server::LocalSymbolViewInput,
    ) -> Result<(), RiftError> {
        let relation = match selected.facet {
            RelationshipFacet::Calls => SymbolRelation::Calls,
            RelationshipFacet::References => SymbolRelation::References,
            _ => return Err(coverage_failure()),
        };
        for (source, target) in &self.local_edges {
            dataset.insert(GraphFact::Relation {
                source,
                relation,
                target,
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
        for page in &self.pages {
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
                        relation,
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
        self.pages.push(page);
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
        let reads = &resolved.published.reads;
        let configuration = resolved.published.configuration.global_configuration();
        let mut acquired = AcquiredAdjacency::default();
        let mut remote = None;
        let mut local = None;
        let mut remaining = RelationshipReadLimits {
            requests_max: REQUESTS_MAX,
            records_max: RECORDS_MAX,
            bytes_max: u64::try_from(limits.literal_bytes_max()).unwrap_or(u64::MAX),
        };
        let mut frontier = BTreeSet::from([selected.seed.clone()]);
        let mut visited = BTreeSet::new();
        let mut depth = 0u32;
        while !frontier.is_empty() && selected.depth.is_none_or(|maximum| depth < maximum) {
            let mut remote_seeds = Vec::new();
            for seed in std::mem::take(&mut frontier) {
                let identity = SymbolIdentity::parse(seed.as_str())?;
                if !identity.owner().is_local() {
                    if selected.direction != TraversalDirection::Outgoing || !configuration.enabled
                    {
                        return Ok(Err(unavailable(
                            SymbolUnavailableReason::InsufficientCoverage,
                        )));
                    }
                    remote_seeds.push(seed);
                    continue;
                }
                if resolved.stale.is_some() {
                    return Ok(Err(unavailable(
                        SymbolUnavailableReason::InsufficientCoverage,
                    )));
                }
                remaining.requests_max = remaining
                    .requests_max
                    .checked_sub(1)
                    .ok_or_else(coverage_failure)?;
                let (engines, roots) = match &local {
                    Some(value) => value,
                    None => {
                        let engines = self.engine_pool_for(&resolved.published).await;
                        let roots = if selected.direction == TraversalDirection::Outgoing {
                            let reads = std::sync::Arc::clone(reads);
                            let engines = std::sync::Arc::clone(&engines);
                            self.blocking
                                .run("graph callee package roots", move || {
                                    Ok(rift_server::CalleeRoots::read(&reads, &engines))
                                })
                                .await?
                        } else {
                            rift_server::CalleeRoots::default()
                        };
                        local.insert((engines, roots))
                    }
                };
                let walk_deadline = deadline.walk_end(resolved.capture_elapsed);
                visited.insert(seed.clone());
                match (selected.direction, selected.facet) {
                    (TraversalDirection::Incoming, RelationshipFacet::References) => {
                        let occurrences = Box::pin(rift_server::graph_references(
                            reads,
                            engines,
                            &seed,
                            walk_deadline,
                        ))
                        .await?;
                        charge_records(&mut remaining, occurrences.len())?;
                        for occurrence in occurrences {
                            if let Some(caller) = &occurrence.enclosing {
                                acquired.local_edges.push((caller.clone(), seed.clone()));
                                frontier.insert(caller.clone());
                            }
                            acquired.local.push(occurrence);
                        }
                    }
                    (TraversalDirection::Outgoing, RelationshipFacet::Calls) => {
                        let mut calls = Box::pin(rift_server::graph_calls(
                            reads,
                            engines,
                            roots,
                            &seed,
                            walk_deadline,
                        ))
                        .await?;
                        if !calls.packages.is_empty() {
                            if !configuration.enabled {
                                return Ok(Err(unavailable(
                                    SymbolUnavailableReason::InsufficientCoverage,
                                )));
                            }
                            let callees: Vec<_> = calls
                                .packages
                                .iter()
                                .map(|(callee, _)| callee.clone())
                                .collect();
                            let client = self.global.client(&configuration).await?;
                            let declarations =
                                crate::global::callee_declarations(&client, &callees, &[]).await?;
                            for (callee, occurrences) in calls.packages {
                                let declaration = declarations
                                    .declaration(&callee)
                                    .ok_or_else(coverage_failure)?;
                                let identity = SymbolIdentity::parse(declaration.id.as_str())?;
                                let exact =
                                    workspace_release(reads.dependency_context(), identity.owner());
                                if exact.as_ref() != Some(&declaration.revision) {
                                    return Ok(Err(unavailable(
                                        SymbolUnavailableReason::InsufficientCoverage,
                                    )));
                                }
                                calls.local.push((declaration.id, occurrences));
                            }
                        }
                        charge_records(&mut remaining, calls.local.len())?;
                        for (target, ranges) in calls.local {
                            charge_records(&mut remaining, ranges.len())?;
                            acquired.local_edges.push((seed.clone(), target.clone()));
                            frontier.insert(target.clone());
                            if selected.usages {
                                for source in ranges {
                                    acquired.local.push(QueryOccurrence {
                                        target: target.clone(),
                                        enclosing: Some(seed.clone()),
                                        source,
                                        facet: RelationshipFacet::Calls,
                                    });
                                }
                            }
                        }
                    }
                    _ => {
                        return Ok(Err(unavailable(
                            SymbolUnavailableReason::InsufficientCoverage,
                        )));
                    }
                }
            }

            if !remote_seeds.is_empty() {
                let remote = match &remote {
                    Some(remote) => remote,
                    None => {
                        let mut domain = exact_domain(reads.dependency_context())?;
                        if domain.packages.is_empty() && domain.runtimes.is_empty() {
                            return Ok(Err(unavailable(
                                SymbolUnavailableReason::InsufficientCoverage,
                            )));
                        }
                        let client = self.global.client(&configuration).await?;
                        let capabilities = client.get_capabilities().await?;
                        let bounds = client.relationship_bounds(
                            &capabilities,
                            selected.direction,
                            &[selected.facet],
                        )?;
                        acquired.revisions = std::mem::take(&mut domain.revisions);
                        remote.insert(RemoteSelection {
                            client,
                            domain,
                            seeds_max: usize::try_from(bounds.max_seeds).unwrap_or(usize::MAX),
                        })
                    }
                };
                for seeds in remote_seeds.chunks(remote.seeds_max) {
                    for seed in seeds {
                        if !acquired
                            .revisions
                            .contains_key(SymbolIdentity::parse(seed.as_str())?.owner())
                        {
                            return Ok(Err(unavailable(
                                SymbolUnavailableReason::InsufficientCoverage,
                            )));
                        }
                        visited.insert(seed.clone());
                    }
                    let request = RelationshipSearchRequest {
                        relationships: RelationshipSelection {
                            seeds: seeds.to_vec(),
                            direction: selected.direction,
                            facets: vec![selected.facet],
                        },
                        packages: remote.domain.packages.clone(),
                        runtimes: remote.domain.runtimes.clone(),
                        include: if selected.source {
                            vec![RelationshipInclude::Source]
                        } else {
                            Vec::new()
                        },
                        view: acquired.view.as_ref().map(|view| view.id.clone()),
                    };
                    let page = remote
                        .client
                        .relationship_pages_with_admission(&request, remaining, || {
                            remaining.requests_max = remaining
                                .requests_max
                                .checked_sub(1)
                                .ok_or_else(coverage_failure)?;
                            Ok(())
                        })
                        .await?;
                    let page = match page {
                        RelationshipSearchResult::Page(page) => page,
                        RelationshipSearchResult::Unavailable(reason) => return Ok(Err(reason)),
                    };
                    if acquired
                        .view
                        .as_ref()
                        .is_some_and(|view| view != &page.view)
                        || acquired.pages.first().is_some_and(|first| {
                            first.analyzer_revision != page.analyzer_revision
                                || first.corpus_revision != page.corpus_revision
                        })
                    {
                        return Ok(Err(unavailable(SymbolUnavailableReason::ViewContext)));
                    }
                    acquired.admit_remote(page, &mut remaining, &mut frontier)?;
                }
            }
            frontier.retain(|seed| !visited.contains(seed));
            depth = depth.checked_add(1).ok_or_else(coverage_failure)?;
        }
        if local.is_some()
            && !self
                .engine_tree_matches(&resolved.published)
                .await
                .map_err(|_| coverage_failure())?
        {
            return Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
        }
        Ok(Ok(acquired))
    }
}

struct RemoteSelection {
    client: rift_cloud_client::GlobalClient,
    domain: ExactDomain,
    seeds_max: usize,
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

fn unavailable(reason: SymbolUnavailableReason) -> SearchUnavailable {
    SearchUnavailable::Unavailable {
        reason,
        view: None,
        warnings: Vec::new(),
    }
}
