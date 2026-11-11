//! Acquisition of complete adjacency from the selected providers.

use std::collections::BTreeSet;

use rift_cloud_client::relationships::RelationshipReadLimits;
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{
    PackageIdentity, ReadWarning, RelationshipFacet, SymbolId, TraversalDirection,
};
use rift_protocol::relationship_read::{
    QueryOccurrence, RelationshipInclude, RelationshipSearchPage, RelationshipSearchRequest,
    RelationshipSearchResult, RelationshipSelection,
};
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
    revision: Option<RevisionIri>,
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
        for occurrence in &self.local {
            if let Some(enclosing) = &occurrence.enclosing {
                dataset.insert(GraphFact::Relation {
                    source: enclosing,
                    relation,
                    target: &occurrence.target,
                })?;
            }
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
            let Some(revision) = &self.revision else {
                return Err(coverage_failure());
            };
            for item in &page.symbols {
                if let Some(symbol) = &item.symbol.id {
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
}

impl RiftMcp {
    pub(super) async fn acquire_adjacency(
        &self,
        resolved: &ResolvedWorkspace,
        selected: &AdjacencySelection,
        limits: GraphLimits,
        deadline: RequestDeadline,
    ) -> Result<Result<AcquiredAdjacency, SearchUnavailable>, RiftError> {
        let identity = SymbolIdentity::parse(selected.seed.as_str())?;
        if identity.owner().is_local() {
            if resolved.stale.is_some()
                || selected.direction != TraversalDirection::Incoming
                || selected.facet != RelationshipFacet::References
                || selected.depth != Some(1)
            {
                return Ok(Err(unavailable(
                    SymbolUnavailableReason::InsufficientCoverage,
                )));
            }
            let engines = self.engine_pool_for(&resolved.published).await;
            let local = rift_server::graph_references(
                &resolved.published.reads,
                &engines,
                &selected.seed,
                deadline.walk_end(resolved.capture_elapsed),
            )
            .await?;
            if !self
                .engine_tree_matches(&resolved.published)
                .await
                .map_err(|_| coverage_failure())?
            {
                return Ok(Err(unavailable(
                    SymbolUnavailableReason::InsufficientCoverage,
                )));
            }
            return Ok(Ok(AcquiredAdjacency {
                local,
                ..AcquiredAdjacency::default()
            }));
        }
        let owner @ SymbolOwner::Package {
            manager,
            registry,
            name,
        } = identity.owner()
        else {
            return Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
        };
        if selected.direction != TraversalDirection::Outgoing {
            return Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
        }
        let configuration = resolved.published.configuration.global_configuration();
        let reads = &resolved.published.reads;
        let Some(revision) =
            workspace_release(reads.dependency_context(), owner).filter(|_| configuration.enabled)
        else {
            return Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
        };
        let package = PackageIdentity {
            manager: manager.to_string(),
            registry: registry.as_str().to_owned(),
            name: name.to_string(),
            version: revision
                .release_version()
                .ok_or_else(coverage_failure)?
                .to_owned(),
        };
        let client = self.global.client(&configuration).await?;
        let capabilities = client.get_capabilities().await?;
        let facets = [selected.facet];
        let bounds = client.relationship_bounds(&capabilities, selected.direction, &facets)?;
        let seeds_max = usize::try_from(bounds.max_seeds).unwrap_or(usize::MAX);
        let mut acquired = AcquiredAdjacency {
            revision: Some(RevisionIri::parse(&revision.to_string())?),
            ..AcquiredAdjacency::default()
        };
        let mut remaining_requests = REQUESTS_MAX;
        let mut remaining_bytes = u64::try_from(limits.literal_bytes_max()).unwrap_or(u64::MAX);
        let mut remaining_records = RECORDS_MAX;
        let mut frontier = BTreeSet::from([selected.seed.clone()]);
        let mut visited = BTreeSet::new();
        let mut depth = 0u32;
        while !frontier.is_empty() && selected.depth.is_none_or(|maximum| depth < maximum) {
            let seeds = Vec::from_iter(std::mem::take(&mut frontier));
            for chunk in seeds.chunks(seeds_max) {
                for seed in chunk {
                    if SymbolIdentity::parse(seed.as_str())?.owner() != owner {
                        return Ok(Err(unavailable(
                            SymbolUnavailableReason::InsufficientCoverage,
                        )));
                    }
                    visited.insert(seed.clone());
                }
                let request = RelationshipSearchRequest {
                    relationships: RelationshipSelection {
                        seeds: chunk.to_vec(),
                        direction: selected.direction,
                        facets: facets.to_vec(),
                    },
                    packages: vec![package.clone()],
                    runtimes: Vec::new(),
                    include: if selected.source {
                        vec![RelationshipInclude::Source]
                    } else {
                        Vec::new()
                    },
                    view: acquired.view.as_ref().map(|view| view.id.clone()),
                };
                let page = client
                    .relationship_pages_with_admission(
                        &request,
                        RelationshipReadLimits {
                            requests_max: remaining_requests,
                            records_max: remaining_records,
                            bytes_max: remaining_bytes,
                        },
                        || {
                            remaining_requests = remaining_requests
                                .checked_sub(1)
                                .ok_or_else(coverage_failure)?;
                            Ok(())
                        },
                    )
                    .await?;
                let mut page = match page {
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
                let maximum = usize::try_from(remaining_bytes).unwrap_or(usize::MAX);
                let charged = crate::proxy::encode_bounded(&page, maximum)
                    .map_err(super::encoding_failure)?
                    .len();
                remaining_bytes = remaining_bytes
                    .checked_sub(u64::try_from(charged).map_err(|_| coverage_failure())?)
                    .ok_or_else(coverage_failure)?;
                for group in &page.groups {
                    let records: BTreeSet<_> = group
                        .relationships
                        .iter()
                        .map(|record| &record.id)
                        .chain(group.occurrences.iter().map(|record| &record.id))
                        .collect();
                    remaining_records = remaining_records
                        .checked_sub(u64::try_from(records.len()).map_err(|_| coverage_failure())?)
                        .ok_or_else(coverage_failure)?;
                    for record in &group.relationships {
                        let next: &SymbolId = &record.relationship.to;
                        if !visited.contains(next) {
                            frontier.insert(next.clone());
                        }
                    }
                }
                acquired.view.get_or_insert_with(|| page.view.clone());
                for warning in page.warnings.drain(..) {
                    if !acquired.warnings.contains(&warning) {
                        acquired.warnings.push(warning);
                    }
                }
                acquired.pages.push(page);
            }
            frontier.retain(|seed| !visited.contains(seed));
            depth = depth.checked_add(1).ok_or_else(coverage_failure)?;
        }
        Ok(Ok(acquired))
    }
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
