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
        let requirements = requirements.clone();
        self.query_work(execution.budget, execution.deadline, move |budget| {
            restore_selected_queries(
                selected,
                &requirements,
                answer_only,
                selection.as_ref(),
                limits,
                budget,
            )
        })
        .await
    }
}

fn restore_selected_queries(
    selected: Vec<SelectedResult>,
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
    for selected in selected {
        let (metadata, restored) = crate::proxy::query_replay::restore(&selected.captured, budget)?;
        let mut bindings = crate::proxy::query_replay::restore_bindings(
            &selected.captured,
            &metadata,
            selected.services,
            budget,
        )?;
        let covered = if answer_only {
            !restored.is_empty()
        } else {
            crate::proxy::query_replay::covers_retained_bindings(
                &restored,
                requirements,
                &bindings,
                selection,
                limits,
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
        for (_, body, identities) in restored {
            selected_bodies.push((
                body,
                rift_search::sparql::QueryServices::default(),
                Some(identities),
            ));
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
