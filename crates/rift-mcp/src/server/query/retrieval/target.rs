use rift_error::RiftError;
use rift_protocol::read::{SearchHit, SearchHitTarget, Symbol};
use rift_search::query::QueryProviderEndpoint;

use super::super::{QueryAcquisition, acquisition_failure};

impl QueryAcquisition {
    pub(in crate::server::query) fn bind_targets(
        mut self,
        budget: &mut rift_server::QueryFactBudget<'_>,
    ) -> Result<Self, RiftError> {
        if self.contributions.len() != self.services.retrievals.len() {
            return Err(acquisition_failure(
                "retrieval record associations do not match selected rows",
            ));
        }
        let mut services = std::mem::take(&mut self.services);
        for (service, contributions) in services
            .retrievals
            .iter_mut()
            .zip(std::mem::take(&mut self.contributions))
        {
            if contributions.len() != service.result.results.len() {
                return Err(acquisition_failure(
                    "retrieval record associations do not match selected rows",
                ));
            }
            for (hit, keys) in service.result.results.iter().zip(contributions) {
                budget.check()?;
                let target = self
                    .bind_original_target(hit, &keys, budget)?
                    .map(|endpoint| (0, endpoint));
                budget.charge(&target)?;
                service.targets.push(target);
            }
        }
        self.services = services;
        Ok(self)
    }

    pub(in crate::server::query) fn bind_original_target(
        &self,
        hit: &SearchHit,
        keys: &[rift_core::ContributionKey],
        budget: &mut rift_server::QueryFactBudget<'_>,
    ) -> Result<Option<QueryProviderEndpoint>, RiftError> {
        let SearchHitTarget::Symbol { symbol } = &hit.hit else {
            if keys.is_empty() {
                return Ok(None);
            }
            return Err(acquisition_failure(
                "retrieval record association has a different result target",
            ));
        };
        if keys.is_empty() {
            validate_canonical_symbol(
                symbol,
                hit.path.as_ref().map(|path| path.0.as_str()),
                hit.unit
                    .as_ref()
                    .map(rift_protocol::read::SourceUnitId::as_str),
            )?;
            return Ok(None);
        }
        let selection = self
            .material
            .captured_provider_selection()
            .ok_or_else(|| acquisition_failure("original provider selection is unavailable"))?;
        let mut found = None;
        for (provider, snapshot) in self.facts.providers.iter().enumerate() {
            budget.check()?;
            if &snapshot.context.selection != selection
                || snapshot.context.root != selection.root
                || snapshot.context.owner != selection.owner
            {
                continue;
            }
            for record in &snapshot.records {
                budget.check()?;
                if !keys.iter().all(|key| record.contributions.contains(key)) {
                    continue;
                }
                if found.is_some()
                    || self
                        .facts
                        .symbols
                        .get(record.symbol_index)
                        .is_none_or(|selected| selected.symbol != **symbol)
                {
                    return Err(acquisition_failure(
                        "retrieval symbol differs from its original selected record",
                    ));
                }
                budget.charge(record)?;
                found = Some(QueryProviderEndpoint {
                    provider: Some(provider),
                    target: record.target().ok_or_else(|| {
                        rift_error::errors::server::read_unavailable()
                            .operation("query acquisition")
                            .detail(format!(
                                "retrieval symbol {:?} has no original selected record target",
                                symbol.name,
                            ))
                            .error()
                    })?,
                });
            }
        }
        found.map(Some).ok_or_else(|| {
            rift_error::errors::server::read_unavailable().operation("query acquisition")
                .detail(format!("retrieval symbol {:?} has no original selected record among {} providers for {} contribution keys",
                    symbol.name, self.facts.providers.len(), keys.len())).error()
        })
    }
}

fn validate_canonical_symbol(
    symbol: &Symbol,
    path: Option<&str>,
    unit: Option<&str>,
) -> Result<(), RiftError> {
    let identity = symbol.id.as_ref().ok_or_else(|| {
        rift_error::errors::server::read_unavailable()
            .operation("query acquisition")
            .detail(format!(
                "retrieval symbol {:?} has no canonical identity or original selected record: language {:?}, kind {:?}, origin {:?}, path {path:?}, unit {unit:?}",
                symbol.name, symbol.language, symbol.kind, symbol.origin,
            ))
            .error()
    })?;
    rift_search::query::QueryProviderTarget::canonical(identity)?;
    Ok(())
}
