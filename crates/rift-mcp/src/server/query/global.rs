//! Request-owned global adjacency and exact facts under one selected publication.

use rift_cloud_client::GlobalClient;
use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{
    PackageIdentity, ReadWarning, RelationshipFacet, RuntimeIdentity, SymbolId, TraversalDirection,
};
use rift_protocol::relationship_read::{
    RelationshipCapability, RelationshipSearchRequest, RelationshipSearchResult,
    RelationshipSelection,
};
use rift_protocol::symbol_read::CapturedView;
use rift_server::EngineQuery;

use super::{RiftMcp, acquisition_failure, incomplete_retrieval};

#[derive(Default)]
pub(super) struct GlobalQuery {
    client: Option<GlobalClient>,
    capabilities: Vec<RelationshipCapability>,
    packages: Vec<PackageIdentity>,
    runtimes: Vec<RuntimeIdentity>,
    domain_warnings: Vec<ReadWarning>,
    selected: bool,
    source: bool,
    pub(super) view: Option<CapturedView>,
    pub(super) warnings: Vec<ReadWarning>,
}

impl GlobalQuery {
    pub(super) fn from_selected(
        selected: &[super::super::candidates::QueryGlobalSelection],
        configuration: &rift_protocol::configuration::GlobalConfiguration,
        source: bool,
    ) -> Result<Self, RiftError> {
        let mut result = Self {
            source,
            ..Self::default()
        };
        for source in selected {
            if &source.configuration != configuration
                || result
                    .view
                    .as_ref()
                    .is_some_and(|view| view != &source.view)
            {
                return Err(acquisition_failure(
                    "required global facts belong to different retained source contexts",
                ));
            }
            result.include_view(&source.view)?;
            for owner in &source.owners {
                result.include_owner(owner);
            }
            result.selected = true;
        }
        Ok(result)
    }

    pub(super) fn selection(
        &self,
        configuration: &rift_protocol::configuration::GlobalConfiguration,
    ) -> Result<Option<super::super::candidates::QueryGlobalSelection>, RiftError> {
        let Some(view) = self.view.clone() else {
            return Ok(None);
        };
        let mut owners = self
            .packages
            .iter()
            .map(PackageIdentity::owner)
            .collect::<Result<Vec<_>, _>>()?;
        owners.extend(self.runtimes.iter().map(|runtime| SymbolOwner::Runtime {
            runtime: runtime.runtime.clone(),
            version: runtime.version.clone(),
        }));
        Ok(Some(super::super::candidates::QueryGlobalSelection {
            owners,
            view,
            configuration: configuration.clone(),
        }))
    }

    pub(super) fn supports(
        &self,
        direction: TraversalDirection,
        facets: &[RelationshipFacet],
    ) -> bool {
        !facets.is_empty()
            && self.capabilities.iter().any(|operation| {
                operation.direction == direction
                    && facets.iter().all(|facet| operation.facets.contains(facet))
            })
    }

    pub(super) fn supported_facets(
        &self,
        direction: TraversalDirection,
        facets: &[RelationshipFacet],
    ) -> Vec<RelationshipFacet> {
        facets
            .iter()
            .copied()
            .filter(|facet| self.supports(direction, &[*facet]))
            .collect()
    }

    pub(super) async fn prepare(
        &mut self,
        mcp: &RiftMcp,
        resolved: &super::super::ResolvedWorkspace,
        query: &EngineQuery<'_>,
        deadline: super::super::RequestDeadline,
    ) -> Result<(), RiftError> {
        if self.client.is_some() {
            return Ok(());
        }
        let configuration = resolved.published.configuration.global_configuration();
        if !configuration.enabled {
            return rift_error::errors::server::read_unsupported()
                .capability("dependency relationships while global access is disabled")
                .fail();
        }
        let client = tokio::time::timeout_at(deadline.at(), mcp.global.client(&configuration))
            .await
            .map_err(|_| acquisition_failure("global client exceeded the request deadline"))??;
        let requests = query.provider_requests();
        let client = client.with_request_admission(move || requests.claim());
        let capabilities = tokio::time::timeout_at(deadline.at(), client.get_capabilities())
            .await
            .map_err(|_| {
                acquisition_failure("global capabilities exceeded the request deadline")
            })??;
        self.capabilities = GlobalClient::relationship_capabilities(&capabilities)?;
        self.client = Some(client);
        Ok(())
    }

    async fn select_domain(
        &mut self,
        mcp: &RiftMcp,
        resolved: &super::super::ResolvedWorkspace,
        seeds: &[SymbolId],
        deadline: super::super::RequestDeadline,
    ) -> Result<(), RiftError> {
        if self.selected {
            return Ok(());
        }
        let read = crate::global::ReadContext::snapshot(&resolved.published.reads);
        let configuration = resolved.published.configuration.global_configuration();
        let client = self
            .client
            .clone()
            .ok_or_else(|| acquisition_failure("global client is absent"))?;
        let route = tokio::time::timeout_at(
            deadline.at(),
            mcp.global.route_with_client(&configuration, &read, client),
        )
        .await
        .map_err(|_| acquisition_failure("global selection exceeded the request deadline"))?;
        if route.service_warnings().iter().any(incomplete_retrieval) {
            return Err(acquisition_failure("global selection is unavailable"));
        }
        self.domain_warnings = route.warnings();
        self.packages = route
            .remote_packages
            .into_iter()
            .map(|package| PackageIdentity {
                manager: package.manager,
                registry: package.registry,
                name: package.name,
                version: package.version,
            })
            .collect();
        for seed in seeds {
            self.include_owner(SymbolIdentity::parse(seed.as_str())?.owner());
        }
        self.selected = true;
        Ok(())
    }

    fn include_owner(&mut self, owner: &SymbolOwner) {
        match owner {
            SymbolOwner::Package {
                manager,
                registry,
                name,
                version,
            } => {
                let package = PackageIdentity {
                    manager: manager.clone(),
                    registry: registry.clone(),
                    name: name.clone(),
                    version: version.clone(),
                };
                if !self.packages.contains(&package) {
                    self.packages.push(package);
                }
            }
            SymbolOwner::Runtime { runtime, version } => {
                let runtime = RuntimeIdentity {
                    runtime: runtime.clone(),
                    version: version.clone(),
                };
                if !self.runtimes.contains(&runtime) {
                    self.runtimes.push(runtime);
                }
            }
            _ => {}
        }
    }

    pub(super) async fn acquire(
        &mut self,
        mcp: &RiftMcp,
        resolved: &super::super::ResolvedWorkspace,
        query: &mut EngineQuery<'_>,
        seeds: &[SymbolId],
        direction: TraversalDirection,
        facets: &[RelationshipFacet],
        deadline: super::super::RequestDeadline,
    ) -> Result<bool, RiftError> {
        self.prepare(mcp, resolved, query, deadline).await?;
        if !self.supports(direction, facets) {
            return Ok(false);
        }
        self.select_domain(mcp, resolved, seeds, deadline).await?;
        if direction == TraversalDirection::Incoming && !self.domain_warnings.is_empty() {
            return Err(acquisition_failure(
                "the selected incoming package domain is incomplete",
            ));
        }
        let request = RelationshipSearchRequest {
            relationships: RelationshipSelection {
                seeds: seeds.to_vec(),
                direction,
                facets: facets.to_vec(),
            },
            packages: if self.view.is_none() {
                self.packages.clone()
            } else {
                Vec::new()
            },
            runtimes: if self.view.is_none() {
                self.runtimes.clone()
            } else {
                Vec::new()
            },
            include: if self.source {
                vec![rift_protocol::read::GetSymbolInclude::Source]
            } else {
                Vec::new()
            },
            view: self.view.as_ref().map(|view| view.id.clone()),
        };
        let configuration = resolved
            .published
            .configuration
            .search_configuration()
            .query;
        let limits = rift_cloud_client::relationships::RelationshipReadLimits {
            requests_max: query.remaining_provider_requests(),
            records_max: configuration.statements,
            bytes_max: configuration.fact_size.bytes(),
        };
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| acquisition_failure("global client is absent"))?;
        let result =
            tokio::time::timeout_at(deadline.at(), client.relationship_pages(&request, limits))
                .await
                .map_err(|_| {
                    acquisition_failure("global relationships exceeded the request deadline")
                })??;
        let RelationshipSearchResult::Page(page) = result else {
            return Err(acquisition_failure(
                "the selected global relationship facts are unavailable",
            ));
        };
        self.include_view(&page.view)?;
        if page.warnings.iter().any(incomplete_retrieval) {
            return Err(acquisition_failure(
                "the selected global relationship facts are incomplete",
            ));
        }
        self.warnings.extend(page.warnings);
        query.admit_package_facts(rift_search::query::QueryFacts {
            symbols: page.symbols,
            sources: page.sources,
            ..rift_search::query::QueryFacts::default()
        })?;
        for group in page.groups {
            query.admit_relationship_records(
                &group.seed,
                direction,
                facets,
                group.relationships,
                group.occurrences,
            )?;
        }
        Ok(true)
    }

    fn include_view(&mut self, view: &CapturedView) -> Result<(), RiftError> {
        if self.view.as_ref().is_some_and(|selected| selected != view) {
            return Err(acquisition_failure(
                "the selected global publication changed during query acquisition",
            ));
        }
        crate::proxy::captured_expiry(view)?;
        if self.view.is_none() {
            self.view = Some(view.clone());
        }
        Ok(())
    }
}

pub(super) fn released(identity: &SymbolId) -> Result<bool, RiftError> {
    Ok(matches!(
        SymbolIdentity::parse(identity.as_str())?.owner(),
        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facet_selection_preserves_required_local_operations() {
        let query = GlobalQuery {
            capabilities: vec![RelationshipCapability {
                direction: TraversalDirection::Outgoing,
                facets: vec![RelationshipFacet::Calls],
            }],
            ..GlobalQuery::default()
        };
        let required = [RelationshipFacet::Calls, RelationshipFacet::References];
        assert!(!query.supports(TraversalDirection::Outgoing, &required));
        assert_eq!(
            query.supported_facets(TraversalDirection::Outgoing, &required),
            [RelationshipFacet::Calls]
        );
        assert!(
            query
                .supported_facets(TraversalDirection::Incoming, &required)
                .is_empty()
        );
        assert!(!GlobalQuery::default().supports(TraversalDirection::Outgoing, &required));
        assert!(!query.supports(TraversalDirection::Outgoing, &[]));
    }

    #[test]
    fn incomplete_provider_selection_cannot_answer_complete_query() {
        for warning in [
            ReadWarning::QueryNarrowed { terms_max: 8 },
            ReadWarning::LexicalRankingTruncated { matches_max: 8 },
            ReadWarning::PackageAbsent {
                package: PackageIdentity {
                    manager: "cargo".to_owned(),
                    registry: "crates.io".to_owned(),
                    name: "demo".to_owned(),
                    version: "1.0.0".to_owned(),
                },
            },
            ReadWarning::PackageContextDegraded {
                resolver: "cargo".to_owned(),
                reason: "dependency context is incomplete".to_owned(),
            },
            ReadWarning::HistoryStoreFilling {
                analyzed: 1,
                total: 2,
                detail: "history is incomplete".to_owned(),
            },
        ] {
            assert!(incomplete_retrieval(&warning));
        }
        assert!(!incomplete_retrieval(&ReadWarning::SymbolNotFound {
            name: "absent".to_owned(),
            alternatives: Vec::new(),
            detail: None,
        }));
    }
}
