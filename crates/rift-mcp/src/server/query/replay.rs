//! Retained fact bodies bypass current acquisition and preserve original owners.

use rift_error::RiftError;
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::SymbolUnavailableReason;
use rift_search::sparql::FactRequirements;

use super::facts::{FactAcquisitions, QueryFactSelection};
use super::{QueryExecution, RiftMcp, ToolFailure, acquisition_failure, failure};

struct SelectedResult {
    captured: rift_server::CapturedQueryView,
    services: Vec<(String, Option<u64>)>,
}

impl RiftMcp {
    pub(super) async fn restore_fact_queries(
        &self,
        requirements: &FactRequirements,
        captured: Option<rift_server::CapturedQueryView>,
        execution: QueryExecution<'_>,
        plan: Option<&rift_search::sparql::QueryPlan>,
    ) -> Result<Result<FactAcquisitions, SearchUnavailable>, ToolFailure> {
        let mut admission = execution.budget.clone();
        let selected = match self.select_query_results(requirements, captured, &mut admission)? {
            Ok(selected) => selected,
            Err(reason) => return Ok(Err(reason)),
        };
        let answer_only = plan.is_some_and(|plan| {
            plan.requirements() == requirements
                && crate::proxy::query_replay::retained_answer_only(requirements)
        });
        let selection = plan
            .filter(|plan| plan.requirements() == requirements)
            .and_then(rift_search::sparql::QueryPlan::retained_subject_selection);
        admission.charge(&selection).map_err(failure)?;
        let selection = selection.cloned();
        let limits = execution.limits;
        admission.charge(requirements).map_err(failure)?;
        admission
            .charge_material(
                size_of::<FactRequirements>()
                    .checked_add(2 * size_of::<usize>())
                    .ok_or_else(|| failure(retention_failure()))?,
            )
            .map_err(failure)?;
        let requirements = std::sync::Arc::new(requirements.clone());
        let mut prepared = Vec::new();
        for selected in selected {
            let requirements_for_scope = std::sync::Arc::clone(&requirements);
            let (metadata, mut restored, bindings, selected_sources) = self
                .query_work(execution.budget, execution.deadline, move |budget| {
                    let (metadata, restored) =
                        crate::proxy::query_replay::restore(&selected.captured, budget)?;
                    let bindings = crate::proxy::query_replay::restore_bindings(
                        &selected.captured,
                        &metadata,
                        selected.services,
                        budget,
                    )?;
                    let selected_sources = crate::proxy::query_replay::selected_node_providers(
                        &metadata,
                        &requirements_for_scope,
                        budget,
                    )?;
                    Ok((metadata, restored, bindings, selected_sources))
                })
                .await?;
            if !answer_only {
                restored = match self
                    .acquire_restored_sources(
                        restored,
                        selected_sources.as_deref(),
                        &requirements,
                        execution,
                        &mut admission,
                    )
                    .await?
                {
                    Ok(restored) => restored,
                    Err(reason) => return Ok(Err(reason)),
                };
            }
            admission
                .charge_material(size_of::<PreparedResult>())
                .map_err(failure)?;
            prepared
                .try_reserve_exact(1)
                .map_err(|_| failure(retention_failure()))?;
            prepared.push((metadata, restored, bindings, selected_sources));
        }
        self.query_work(execution.budget, execution.deadline, move |budget| {
            restore_selected_queries(
                prepared,
                &requirements,
                answer_only,
                selection.as_ref(),
                limits,
                budget,
            )
        })
        .await
    }
    fn select_query_results(
        &self,
        requirements: &FactRequirements,
        captured: Option<rift_server::CapturedQueryView>,
        admission: &mut super::QueryBudget,
    ) -> Result<Result<Vec<SelectedResult>, SearchUnavailable>, ToolFailure> {
        let mut selected = Vec::<SelectedResult>::new();
        if let Some(captured) = captured {
            admission
                .charge_material(size_of::<SelectedResult>())
                .map_err(failure)?;
            selected
                .try_reserve_exact(1)
                .map_err(|_| failure(retention_failure()))?;
            selected.push(SelectedResult {
                captured,
                services: Vec::new(),
            });
        }
        for service in &requirements.services {
            let Some((view, page)) =
                crate::proxy::query_replay::result_service(service).map_err(failure)?
            else {
                continue;
            };
            let index = if let Some(index) = selected
                .iter()
                .position(|selection| selection.captured.view().id == view)
            {
                index
            } else {
                let Some(captured) = self.captured_query_material(&view, "query facts")? else {
                    return Ok(Err(unavailable()));
                };
                admission
                    .charge_material(size_of::<SelectedResult>())
                    .map_err(failure)?;
                selected
                    .try_reserve_exact(1)
                    .map_err(|_| failure(retention_failure()))?;
                selected.push(SelectedResult {
                    captured,
                    services: Vec::new(),
                });
                selected.len() - 1
            };
            if selected[index]
                .services
                .iter()
                .any(|(known, _)| known == service)
            {
                continue;
            }
            admission.charge(&(service, page)).map_err(failure)?;
            selected[index]
                .services
                .try_reserve_exact(1)
                .map_err(|_| failure(retention_failure()))?;
            selected[index].services.push((service.clone(), page));
        }
        if selected.is_empty() {
            return Ok(Err(unavailable()));
        }
        Ok(Ok(selected))
    }

    async fn acquire_restored_sources(
        &self,
        restored: Vec<RestoredFactBody>,
        selected_sources: Option<&[bool]>,
        requirements: &std::sync::Arc<FactRequirements>,
        execution: QueryExecution<'_>,
        admission: &mut super::QueryBudget,
    ) -> Result<Result<Vec<RestoredFactBody>, SearchUnavailable>, ToolFailure> {
        admission
            .charge_material(
                restored
                    .len()
                    .checked_mul(size_of::<(
                        String,
                        QueryFactSelection,
                        rift_search::sparql::QueryIdentities,
                    )>())
                    .ok_or_else(|| failure(retention_failure()))?,
            )
            .map_err(failure)?;
        let mut updated = Vec::new();
        updated
            .try_reserve_exact(restored.len())
            .map_err(|_| failure(retention_failure()))?;
        for (index, (backend, body, identities)) in restored.into_iter().enumerate() {
            if selected_sources.is_some_and(|selected| !selected[index]) {
                updated.push((backend, body, identities));
                continue;
            }
            let selected_node_provider = selected_sources.is_some_and(|selected| selected[index]);
            let requested = std::sync::Arc::clone(requirements);
            let (backend, body, identities, requested) = self
                .query_work(execution.budget, execution.deadline, move |budget| {
                    let requested = crate::proxy::query_replay::source_requirements(
                        &body,
                        &identities,
                        &requested,
                        selected_node_provider,
                        budget,
                    )?;
                    Ok((backend, body, identities, requested))
                })
                .await?;
            let Some(requested) = requested else {
                updated.push((backend, body, identities));
                continue;
            };
            let row = match self
                .acquire_original_source(backend, body, requested, execution)
                .await?
            {
                Ok(row) => row,
                Err(reason) => return Ok(Err(reason)),
            };
            updated.push(row);
        }
        Ok(Ok(updated))
    }

    async fn acquire_original_source(
        &self,
        backend: String,
        body: QueryFactSelection,
        requested: FactRequirements,
        execution: QueryExecution<'_>,
    ) -> Result<Result<RestoredFactBody, SearchUnavailable>, ToolFailure> {
        if backend != "local" {
            return Ok(Err(unavailable()));
        }
        let original_view = match self.query_fact_view(Some(body.view.id.clone()))? {
            Ok(Some(super::facts::SelectedFactOwner::Local(material, view))) => {
                if material.root().to_str() != Some(body.root.as_str())
                    || material.owner() != &body.owner
                {
                    return Ok(Err(unavailable()));
                }
                material
                    .validate_provider_selection(&body.selection)
                    .map_err(failure)?;
                Some(super::facts::SelectedFactOwner::Local(material, view))
            }
            _ => return Ok(Err(unavailable())),
        };
        let maximum = execution.requests.remaining();
        let acquired = match self
            .acquire_fact_queries(&requested, original_view, execution, None)
            .await?
        {
            Ok(acquired) => acquired,
            Err(reason) => return Ok(Err(reason)),
        };
        let mut acquired = self
            .capture_query_fact_bodies(
                acquired,
                (requested, maximum),
                execution.requests.clone(),
                execution.budget,
                execution.deadline,
            )
            .await?;
        if acquired.len() != 1 {
            return Ok(Err(unavailable()));
        }
        let (added, _, _) = acquired.pop().ok_or_else(|| failure(retention_failure()))?;
        let row = self
            .query_work(execution.budget, execution.deadline, move |budget| {
                let identities = crate::proxy::query_replay::original_identities(
                    &backend, &added, &body, budget,
                )?;
                let mut added = added;
                added.graph = body.graph;
                added.view = body.view;
                Ok((backend, added, identities))
            })
            .await?;
        Ok(Ok(row))
    }
}

type RestoredFactBody = (
    String,
    QueryFactSelection,
    rift_search::sparql::QueryIdentities,
);

type PreparedResult = (
    crate::proxy::query_material::Metadata,
    Vec<(
        String,
        QueryFactSelection,
        rift_search::sparql::QueryIdentities,
    )>,
    rift_search::sparql::QueryServices,
    Option<Vec<bool>>,
);

fn restore_selected_queries(
    selected: Vec<PreparedResult>,
    requirements: &FactRequirements,
    answer_only: bool,
    selection: Option<&rift_search::sparql::RetainedSubjectSelection>,
    limits: rift_search::sparql::QueryLimits,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Result<FactAcquisitions, SearchUnavailable>, RiftError> {
    let mut bodies = Vec::<(
        QueryFactSelection,
        rift_search::sparql::QueryServices,
        Option<rift_search::sparql::QueryIdentities>,
    )>::new();
    for (_metadata, restored, mut bindings, selected_sources) in selected {
        let covered = if answer_only {
            !restored.is_empty()
        } else {
            crate::proxy::query_replay::covers_retained_source_bindings(
                &restored,
                requirements,
                &bindings,
                selection,
                limits,
                selected_sources.as_deref(),
                budget,
            )?
        };
        if !covered {
            return Ok(Err(unavailable()));
        }
        let mut selected_bodies = Vec::new();
        budget.charge_material(
            restored
                .len()
                .checked_mul(size_of::<(
                    QueryFactSelection,
                    rift_search::sparql::QueryServices,
                    Option<rift_search::sparql::QueryIdentities>,
                )>())
                .ok_or_else(retention_failure)?,
        )?;
        selected_bodies
            .try_reserve_exact(restored.len())
            .map_err(|_| retention_failure())?;
        for (index, (_, body, identities)) in restored.into_iter().enumerate() {
            let mut services = rift_search::sparql::QueryServices::default();
            if selected_sources
                .as_ref()
                .is_some_and(|selected| !selected[index])
            {
                crate::proxy::query_replay::include_empty_node_domain(
                    &mut services,
                    &body,
                    &identities,
                    budget,
                )?;
            }
            selected_bodies.push((body, services, Some(identities)));
        }
        let mut restored = selected_bodies;
        let first = restored.first_mut().ok_or_else(retention_failure)?;
        first.1.bindings.append(&mut bindings.bindings);
        let offset = bodies.len();
        for (body, _, _) in &mut restored {
            for service in &mut body.services.retrievals {
                for (owner, _) in service.targets.iter_mut().flatten() {
                    *owner = owner.checked_add(offset).ok_or_else(retention_failure)?;
                }
            }
        }
        let bytes = restored
            .len()
            .checked_mul(size_of::<(
                QueryFactSelection,
                rift_search::sparql::QueryServices,
                Option<rift_search::sparql::QueryIdentities>,
            )>())
            .ok_or_else(retention_failure)?;
        budget.charge_material(bytes)?;
        bodies
            .try_reserve_exact(restored.len())
            .map_err(|_| retention_failure())?;
        bodies.append(&mut restored);
    }
    if crate::proxy::query_replay::conflicting_local_graphs(bodies.iter().map(|(body, _, _)| body))
    {
        return Ok(Err(SearchUnavailable::Unavailable {
            reason: SymbolUnavailableReason::ViewContext,
            view: None,
            warnings: Vec::new(),
        }));
    }
    Ok(Ok(FactAcquisitions::Captured(bodies)))
}

fn unavailable() -> SearchUnavailable {
    SearchUnavailable::Unavailable {
        reason: SymbolUnavailableReason::InsufficientCoverage,
        view: None,
        warnings: Vec::new(),
    }
}

fn retention_failure() -> RiftError {
    acquisition_failure("retained query material is unavailable; repeat the original query")
}
