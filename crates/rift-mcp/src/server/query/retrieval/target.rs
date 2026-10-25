use rift_error::RiftError;
use rift_protocol::read::{SearchHitTarget, Symbol};
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
        let selection = self.material.captured_provider_selection();
        for (service, contributions) in self
            .services
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
                let target = match &hit.hit {
                    SearchHitTarget::Symbol { symbol } if !keys.is_empty() => {
                        let selection = selection.ok_or_else(|| {
                            acquisition_failure("original provider selection is unavailable")
                        })?;
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
                        Some((
                            rift_search::sparql::LOCAL_GRAPH.to_owned(),
                            found.ok_or_else(|| {
                                rift_error::errors::server::read_unavailable()
                                    .operation("query acquisition")
                                    .detail(format!(
                                        "retrieval symbol {:?} has no original selected record among {} providers for {} contribution keys",
                                        symbol.name, self.facts.providers.len(), keys.len(),
                                    ))
                                    .error()
                            })?,
                        ))
                    }
                    SearchHitTarget::Symbol { symbol } => {
                        validate_canonical_symbol(symbol)?;
                        None
                    }
                    _ if keys.is_empty() => None,
                    _ => {
                        return Err(acquisition_failure(
                            "retrieval record association has a different result target",
                        ));
                    }
                };
                budget.charge(&target)?;
                service.targets.push(target);
            }
        }
        Ok(self)
    }
}

fn validate_canonical_symbol(symbol: &Symbol) -> Result<(), RiftError> {
    let identity = symbol.id.as_ref().ok_or_else(|| {
        rift_error::errors::server::read_unavailable()
            .operation("query acquisition")
            .detail(format!(
                "retrieval symbol {:?} has no canonical identity or original selected record",
                symbol.name,
            ))
            .error()
    })?;
    rift_search::query::QueryProviderTarget::canonical(identity)?;
    Ok(())
}
