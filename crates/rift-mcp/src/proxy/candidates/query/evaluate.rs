use rift_error::RiftError;
use rift_search::sparql::{
    QueryDataset, QueryFacts, QueryLimits, QueryPlan, QueryServiceData, QueryServices,
};
use rmcp::handler::server::tool::IntoCallToolResult as _;
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
                prepare_selected_services(
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
                input.capture(views, &root, &owner, &answer, warnings, budget)
            })
            .await?;
        Ok(crate::output::Json(response).into_call_tool_result())
    }
}

pub(crate) fn prepare_selected_services(
    selections: Vec<(
        String,
        crate::server::QueryFactSelection,
        QueryServices,
        Option<rift_search::sparql::QueryIdentities>,
    )>,
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
    let routing = owner_routing(&selections, budget)?;
    let dictionaries = selected_identities(&selections, budget)?;
    let mut captured = Vec::new();
    let mut services = QueryServices::default();
    let facts = QueryFacts::default();
    let mut warnings = Vec::new();
    for ((backend, mut selected, mut producer_services, _), identities) in
        selections.into_iter().zip(dictionaries)
    {
        budget.check()?;
        budget.charge(&selected.facts)?;
        budget.charge(&identities)?;
        if !selected.services.datasets.is_empty() {
            return Err(retained::unavailable());
        }
        budget.charge(&producer_services)?;
        include_selected_datasets(
            &mut services,
            &mut producer_services,
            &selected,
            &identities,
            &backend,
            budget,
        )?;
        budget.charge_material(
            producer_services
                .sources
                .len()
                .checked_mul(size_of::<(
                    rift_protocol::source_read::GetSourceParams,
                    rift_protocol::source_read::GetSourceResult,
                )>())
                .ok_or_else(retained::unavailable)?,
        )?;
        services
            .sources
            .try_reserve_exact(producer_services.sources.len())
            .map_err(|_| retained::unavailable())?;
        services.sources.append(&mut producer_services.sources);
        services.bindings.append(&mut producer_services.bindings);
        for service in &mut producer_services.retrievals {
            for (owner, _) in service.targets.iter_mut().flatten() {
                *owner = *routing.get(*owner).ok_or_else(retained::unavailable)?;
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
    let mut input = retained::Input::prepare(captured, configuration, maximum, budget)?;
    for (request, result) in &services.sources {
        budget.check()?;
        let view = match result {
            rift_protocol::source_read::GetSourceResult::Found { view, .. }
            | rift_protocol::source_read::GetSourceResult::Missing { view, .. } => view,
            rift_protocol::source_read::GetSourceResult::Unavailable { .. } => {
                return Err(retained::unavailable());
            }
        };
        input.include_view(view)?;
        input.include_source(request, result, budget)?;
    }
    Ok((input, facts, services, warnings))
}

fn include_selected_datasets(
    services: &mut QueryServices,
    producer_services: &mut QueryServices,
    selected: &crate::server::QueryFactSelection,
    identities: &rift_search::sparql::QueryIdentities,
    backend: &str,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    if producer_services.datasets.is_empty() {
        services.datasets.push(QueryDataset {
            graph: selected.graph.as_ref().map(|graph| {
                if backend == "local" {
                    graph.clone()
                } else {
                    format!("{}{backend}", rift_search::sparql::EXTRA_PROJECTS)
                }
            }),
            facts: selected.facts.clone(),
            identities: identities.clone(),
            default: true,
        });
    } else {
        budget.charge_material(
            producer_services
                .datasets
                .len()
                .checked_mul(size_of::<QueryDataset>())
                .ok_or_else(retained::unavailable)?,
        )?;
        services
            .datasets
            .try_reserve_exact(producer_services.datasets.len())
            .map_err(|_| retained::unavailable())?;
        for dataset in &mut producer_services.datasets {
            if backend != "local" && dataset.graph.is_some() {
                budget.charge_material(
                    rift_search::sparql::EXTRA_PROJECTS
                        .len()
                        .checked_add(backend.len())
                        .ok_or_else(retained::unavailable)?,
                )?;
                dataset.graph = Some(format!("{}{backend}", rift_search::sparql::EXTRA_PROJECTS));
            }
        }
        services.datasets.append(&mut producer_services.datasets);
    }
    Ok(())
}

fn owner_routing(
    selections: &[(
        String,
        crate::server::QueryFactSelection,
        QueryServices,
        Option<rift_search::sparql::QueryIdentities>,
    )],
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Vec<usize>, RiftError> {
    if super::replay::conflicting_local_graphs(
        selections
            .iter()
            .filter(|row| row.0 == "local")
            .map(|row| &row.1),
    ) {
        return Err(retained::unavailable());
    }
    let bytes = selections
        .len()
        .checked_mul(size_of::<usize>())
        .and_then(|bytes| bytes.checked_add(size_of::<Vec<usize>>()))
        .ok_or_else(retained::unavailable)?;
    budget.charge_material(bytes)?;
    let mut routing = Vec::new();
    routing
        .try_reserve_exact(selections.len())
        .map_err(|_| retained::unavailable())?;
    for index in 0..selections.len() {
        routing.push(index.checked_add(1).ok_or_else(retained::unavailable)?);
    }
    Ok(routing)
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

fn selected_identities(
    selections: &[(
        String,
        crate::server::QueryFactSelection,
        QueryServices,
        Option<rift_search::sparql::QueryIdentities>,
    )],
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Vec<rift_search::sparql::QueryIdentities>, RiftError> {
    budget.charge_material(
        selections
            .len()
            .checked_mul(size_of::<rift_search::sparql::QueryIdentities>())
            .ok_or_else(retained::unavailable)?,
    )?;
    let mut dictionaries = Vec::new();
    dictionaries
        .try_reserve_exact(selections.len())
        .map_err(|_| retained::unavailable())?;
    for (backend, body, services, saved) in selections {
        budget.check()?;
        let same_owner = |candidate: &&(
            String,
            crate::server::QueryFactSelection,
            QueryServices,
            Option<rift_search::sparql::QueryIdentities>,
        )| {
            candidate.0 == *backend
                && candidate.1.root == body.root
                && candidate.1.owner == body.owner
                && candidate.1.selection == body.selection
        };
        let anchor = selections
            .iter()
            .filter(same_owner)
            .find(|row| row.3.is_some())
            .or_else(|| {
                selections
                    .iter()
                    .filter(same_owner)
                    .find(|row| row.1.graph.is_some())
            })
            .or_else(|| {
                selections.iter().find(|row| {
                    row.0 == *backend
                        && row.1.root == body.root
                        && row.1.owner == body.owner
                        && row.1.selection == body.selection
                })
            })
            .ok_or_else(retained::unavailable)?;
        let dictionary = if let Some(saved) = saved {
            if let Some(original) = &anchor.3 {
                check_shared_dictionary(original, saved, budget)?;
            }
            budget.charge(saved)?;
            saved.clone()
        } else {
            let fresh_sources = if anchor.1.graph.is_none() && anchor.3.is_none() {
                fresh_source_identities(backend, body, services, budget)?
            } else {
                None
            };
            let mut dictionary = if let Some(dictionary) = fresh_sources {
                dictionary
            } else if body.graph == anchor.1.graph {
                identity::identities(backend, body, budget)?
            } else {
                identity::identities_for(backend, body, &anchor.1, budget)?
            };
            if let Some(original) = &anchor.3 {
                if dictionary.blank_prefix != original.blank_prefix {
                    return Err(retained::unavailable());
                }
                for (source, public) in &mut dictionary.sources {
                    if let Some((_, saved)) =
                        original.sources.iter().find(|(unit, _)| unit == source)
                    {
                        budget.charge(saved)?;
                        *public = saved.clone();
                    }
                }
                for (symbol, public) in &dictionary.symbols {
                    if original
                        .symbols
                        .iter()
                        .any(|(id, saved)| id == symbol && saved != public)
                    {
                        return Err(retained::unavailable());
                    }
                }
            }
            dictionary
        };
        dictionaries.push(dictionary);
    }
    Ok(dictionaries)
}

fn fresh_source_identities(
    backend: &str,
    body: &crate::server::QueryFactSelection,
    services: &QueryServices,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Option<rift_search::sparql::QueryIdentities>, RiftError> {
    if body.graph.is_some()
        || !body.requirements.required.is_empty()
        || !body.requirements.retrievals.is_empty()
        || body.requirements.source_reads.is_empty()
        || services.sources.is_empty()
    {
        return Ok(None);
    }
    for (request, result) in &services.sources {
        budget.check()?;
        if !result.is_valid_for(request) {
            return Err(retained::unavailable());
        }
    }
    super::source::source_identities(
        backend,
        body,
        services
            .sources
            .iter()
            .map(|(request, _)| (&request.unit, &request.unit)),
        budget,
    )
    .map(Some)
}

fn check_shared_dictionary(
    original: &rift_search::sparql::QueryIdentities,
    selected: &rift_search::sparql::QueryIdentities,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    if original.blank_prefix != selected.blank_prefix {
        return Err(retained::unavailable());
    }
    for (unit, public) in &selected.sources {
        budget.check()?;
        if original
            .sources
            .iter()
            .any(|(known, mapped)| known == unit && mapped != public)
        {
            return Err(retained::unavailable());
        }
    }
    for (id, public) in &selected.symbols {
        budget.check()?;
        if original
            .symbols
            .iter()
            .any(|(known, mapped)| known == id && mapped != public)
        {
            return Err(retained::unavailable());
        }
    }
    Ok(())
}
