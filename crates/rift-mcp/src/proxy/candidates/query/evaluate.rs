use rift_error::{RiftError, errors};
use rift_protocol::read::SourceUnitId;
use rift_search::sparql::{
    QueryDataset, QueryFacts, QueryLimits, QueryPlan, QueryServiceData, QueryServices,
};
use rmcp::model::CallToolResponse;
use std::sync::Arc;

use super::super::RiftProxy;
use super::{acquire, identity, retained};

impl RiftProxy {
    pub(super) async fn evaluate_owned_query(
        &self,
        plan: QueryPlan,
        limits: QueryLimits,
        deadline: tokio::time::Instant,
    ) -> Result<Result<CallToolResponse, rmcp::ErrorData>, RiftError> {
        let budget =
            rift_server::QueryFactBudget::for_request(limits, deadline.into_std(), &|| false)?;
        let requests = rift_server::ProviderRequests::new(self.query_configuration.requests);
        let selections = match self
            .query_inputs(&plan, &budget, &requests, deadline)
            .await?
        {
            acquire::Acquired::Facts(selections) => selections,
            acquire::Acquired::Response(response) => return Ok(*response),
        };
        let originals = plan.retrievals().to_vec();
        let maximum = rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max;
        let configuration = self.query_configuration.clone();
        let (input, facts, services, warnings) = self
            .query_work(&budget, deadline, move |budget| {
                prepare_selections(
                    selections,
                    &originals,
                    limits,
                    configuration,
                    maximum,
                    budget,
                )
            })
            .await?;
        let program = self.query_program.clone().ok_or_else(super::unsupported)?;
        let answer = self
            .query_work(&budget, deadline, move |budget| {
                budget.charge(&services)?;
                rift_server::evaluate_query(
                    program.as_path(),
                    plan,
                    facts,
                    limits,
                    services,
                    deadline.into_std(),
                    &|| budget.check().is_err(),
                )
            })
            .await?;
        let views = Arc::clone(&self.query_views);
        let root = self
            .accepted_root
            .clone()
            .ok_or_else(retained::unavailable)?;
        let owner = self.owner.clone();
        let response = self
            .query_work(&budget, deadline, move |budget| {
                let views = views.get().ok_or_else(retained::unavailable)?;
                budget.charge(&answer)?;
                let response = input.capture(views, &root, &owner, &answer, warnings, budget)?;
                serde_json::to_value(response).map_err(encoding_failure)
            })
            .await?;
        Ok(Ok(rmcp::model::CallToolResult::structured(response).into()))
    }
}

fn prepare_selections(
    selections: Vec<(String, crate::server::QueryFactSelection)>,
    originals: &[rift_protocol::read::SearchParams],
    limits: QueryLimits,
    configuration: rift_protocol::query::QueryConfiguration,
    maximum: usize,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<
    (
        retained::Input,
        QueryFacts,
        QueryServices,
        Vec<rift_protocol::read::ReadWarning>,
    ),
    RiftError,
> {
    let mut captured = Vec::new();
    let mut services = QueryServices::default();
    let mut facts = QueryFacts::default();
    let mut warnings = Vec::new();
    for (backend, mut selected) in selections {
        budget.check()?;
        for service in &mut selected.services.retrievals {
            for hit in &mut service.result.results {
                if hit.unit.is_none()
                    && let Some(path) = &hit.path
                {
                    let unit = rift_core::SourceUnitId::new(
                        rift_core::SourceResolverId::new("project")?,
                        rift_core::SourcePath::new(path.0.as_str())?,
                    )?;
                    hit.unit = Some(SourceUnitId::parse(&unit.to_string())?);
                }
            }
        }
        let identities = identity::identities(&backend, &selected, budget)?;
        budget.charge(&selected.facts)?;
        if backend == "local" {
            facts = selected.facts.clone();
        } else {
            budget.charge(&identities)?;
            services.datasets.push(QueryDataset {
                graph: format!("{}{backend}", rift_search::sparql::EXTRA_PROJECTS),
                facts: selected.facts.clone(),
                identities: identities.clone(),
                default: true,
            });
        }
        budget.charge(&selected.services)?;
        let mut producer_services = selected.services.clone();
        for service in &mut producer_services.retrievals {
            for (graph, _) in service.targets.iter_mut().flatten() {
                *graph = if backend == "local" {
                    rift_search::sparql::LOCAL_GRAPH.to_owned()
                } else {
                    format!("{}{backend}", rift_search::sparql::EXTRA_PROJECTS)
                };
            }
            service.request = acquire::original_request(&service.request, &backend, originals)?;
            for hit in &mut service.result.results {
                crate::proxy::identity::qualify_hit(hit, &backend)?;
                if let Some(unit) = &mut hit.unit
                    && let Some((_, public)) = identities
                        .sources
                        .iter()
                        .find(|(original, _)| original == unit)
                {
                    *unit = public.clone();
                }
            }
            service.result.view = None;
        }
        for service in producer_services.retrievals {
            merge_retrieval(&mut services.retrievals, service, limits, budget)?;
        }
        crate::proxy::identity::qualify_warnings(&mut selected.warnings, &backend)?;
        warnings.extend(selected.warnings.iter().cloned());
        captured.push((backend, selected, identities));
    }
    let input = retained::Input::prepare(captured, configuration, maximum, budget)?;
    Ok((input, facts, services, warnings))
}

fn merge_retrieval(
    results: &mut Vec<QueryServiceData>,
    mut added: QueryServiceData,
    limits: QueryLimits,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    budget.charge(&added)?;
    if let Some(selected) = results
        .iter_mut()
        .find(|selected| selected.request == added.request)
    {
        if selected
            .result
            .results
            .len()
            .checked_add(added.result.results.len())
            .is_none_or(|count| count > limits.result_items_max)
        {
            return Err(retained::unavailable());
        }
        selected.result.results.append(&mut added.result.results);
        selected.targets.append(&mut added.targets);
        selected.result.warnings.append(&mut added.result.warnings);
    } else {
        results.push(added);
    }
    Ok(())
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}
