//! Comparison facts keep the two read services that selected their declarations.

use std::sync::Arc;

use rift_protocol::read::{Pagination, SearchParams, SearchResult};
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::SymbolUnavailableReason;
use rift_search::query::{FactRequirement, FactRequirements};
use rift_search::sparql::{QueryServiceData, QueryServices};

use super::{
    QueryAcquisition, QueryExecution, RiftMcp, ToolFailure, acquisition_failure, requires_engine,
};

impl RiftMcp {
    pub(super) async fn acquire_change_query(
        &self,
        requirements: &FactRequirements,
        request: &SearchParams,
        execution: QueryExecution<'_>,
    ) -> Result<Result<Vec<QueryAcquisition>, SearchUnavailable>, ToolFailure> {
        super::retrieval::validate_retrieval_start(request)?;
        // Historical source does not authorize a new request against today's engine.
        if request.view.is_some() || requires_engine(requirements) {
            return Ok(Err(SearchUnavailable::Unavailable {
                reason: SymbolUnavailableReason::InsufficientCoverage,
                view: None,
                warnings: Vec::new(),
            }));
        }
        let resolved = self
            .published_workspace(rift_protocol::error::ErrorPhase::Read, execution.deadline)
            .await?;
        let revision = self.revision_read(&resolved.published)?;
        let configuration = resolved
            .published
            .configuration
            .accepted_for_tool(rift_protocol::error::ErrorPhase::Read)?;
        let reads = Arc::clone(&resolved.published.reads);
        if let Some(history) = &self.history {
            reads.attach_history_store(history.stored());
        }
        let prepared = resolved.published.preparation.is_none();
        let stale = resolved.stale;
        let requirements = requirements.clone();
        let request = request.clone();
        let requests = execution.requests.clone();
        let limits = execution.limits;
        self.query_work(execution.budget, execution.deadline, move |budget| {
            let change = request
                .change
                .as_ref()
                .ok_or_else(|| acquisition_failure("comparison request is unavailable"))?;
            let snapshot = reads.capture_change(
                &request,
                change,
                revision.limits,
                &revision.visibility,
                (&revision.text_inclusion, &revision.languages),
                budget,
            )?;
            let mut selected = project_change_owners(
                &snapshot,
                &requirements,
                &configuration,
                limits,
                prepared,
                budget,
            )?;
            let service = comparison_service(
                &snapshot,
                &request,
                &selected,
                &requests,
                configuration.search.query.results,
                budget,
            )?;
            let first = selected
                .first_mut()
                .ok_or_else(|| acquisition_failure("comparison source owner is unavailable"))?;
            first.services.retrievals.push(service);
            if matches!(change.head, rift_protocol::read::ChangeHead::Tree(_)) {
                first.warnings.extend(stale);
            }
            Ok(Ok(selected))
        })
        .await
    }
}

fn project_change_owners(
    snapshot: &rift_server::ChangeSnapshot,
    requirements: &FactRequirements,
    configuration: &rift_protocol::configuration::WorkspaceConfiguration,
    limits: rift_search::query::QueryLimits,
    ready: bool,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Vec<QueryAcquisition>, rift_error::RiftError> {
    budget.charge(requirements)?;
    let mut projection = requirements.clone();
    projection.require(FactRequirement::Domain);
    projection.require(FactRequirement::Description);
    let mut selected = Vec::new();
    selected
        .try_reserve_exact(snapshot.read_services().len())
        .map_err(reserve_failure)?;
    for (reads, material) in snapshot
        .read_services()
        .iter()
        .zip(snapshot.view_inputs(ready)?)
    {
        let selection = material.provider_selection(configuration)?;
        budget.charge(&selection)?;
        let material = material.with_provider_selection(selection.clone())?;
        let (mut facts, subjects) =
            material.query_facts_with_subjects(&projection, limits, budget, &selection)?;
        reads.query_histories(&material, &projection, &mut facts, limits, budget)?;
        let complete = projection.requires(FactRequirement::AllHistories)
            && !facts
                .unavailable_families
                .contains(&rift_search::query::QueryFactFamily::History);
        let material = material.with_histories(&facts.histories, complete, limits, budget)?;
        selected.push(QueryAcquisition {
            graph: None,
            facts,
            subjects,
            material,
            services: QueryServices::default(),
            contributions: Vec::new(),
            warnings: Vec::new(),
            upstream: Vec::new(),
        });
    }
    Ok(selected)
}

fn comparison_service(
    snapshot: &rift_server::ChangeSnapshot,
    request: &SearchParams,
    selected: &[QueryAcquisition],
    requests: &rift_server::ProviderRequests,
    maximum: u64,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<QueryServiceData, rift_error::RiftError> {
    let mut result = SearchResult {
        view: None,
        results: Vec::new(),
        pagination: Pagination {
            page_index: 0,
            total_pages: 0,
        },
        warnings: Vec::new(),
    };
    let mut targets = Vec::new();
    let mut page_index = 0u64;
    loop {
        requests.claim()?;
        let mut page = snapshot.page(page_index, budget)?;
        if page.result.pagination.page_index != page_index
            || page.sides.len() != page.result.results.len()
            || page.contributions.len() != page.result.results.len()
            || result
                .results
                .len()
                .checked_add(page.result.results.len())
                .and_then(|count| u64::try_from(count).ok())
                .is_none_or(|count| count > maximum)
        {
            return Err(acquisition_failure(
                "comparison page differs from its original selected rows",
            ));
        }
        for ((hit, keys), side) in page
            .result
            .results
            .iter()
            .zip(&page.contributions)
            .zip(page.sides)
        {
            let owner = selected
                .get(side)
                .ok_or_else(|| acquisition_failure("comparison source owner is unavailable"))?;
            let target = owner
                .bind_original_target(hit, keys, budget)?
                .map(|target| (side, target));
            budget.charge(&target)?;
            targets.try_reserve(1).map_err(reserve_failure)?;
            targets.push(target);
        }
        result
            .results
            .try_reserve(page.result.results.len())
            .map_err(reserve_failure)?;
        result.results.append(&mut page.result.results);
        for warning in page.result.warnings {
            if !result.warnings.contains(&warning) {
                result.warnings.push(warning);
            }
        }
        page_index = page_index
            .checked_add(1)
            .ok_or_else(|| acquisition_failure("comparison page exceeds the platform bound"))?;
        if page_index >= page.result.pagination.total_pages {
            break;
        }
    }
    result.pagination.total_pages = u64::from(!result.results.is_empty());
    budget.charge(request)?;
    Ok(QueryServiceData {
        request: request.clone(),
        result,
        targets,
    })
}

fn reserve_failure(error: impl std::fmt::Display) -> rift_error::RiftError {
    rift_error::errors::server::read_unavailable()
        .operation("query acquisition")
        .detail(error.to_string())
        .error()
}
