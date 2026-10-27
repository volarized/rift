//! Authoritative query facts acquired through the existing local read owners.

use rift_error::{RiftError, errors};
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::ReadWarning;
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use rift_search::sparql::{FactRequirements, LOCAL_GRAPH, QueryFacts, QueryServices};
use rift_server::QueryExchange;
use serde::{Deserialize, Serialize};

use super::{
    QueryBudget, QueryExecution, RiftMcp, ToolFailure, acquisition_failure, failure, query_limits,
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryFactRequest {
    pub(crate) requirements: FactRequirements,
    pub(crate) view: Option<CapturedViewId>,
    pub(crate) requests: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryFactSelection {
    pub(crate) root: String,
    pub(crate) owner: SymbolOwner,
    pub(crate) view: CapturedView,
    pub(crate) selection: rift_search::query::QueryProviderSelection,
    pub(crate) requests: u64,
    #[serde(deserialize_with = "required_graph")]
    pub(crate) graph: Option<String>,
    pub(crate) requirements: FactRequirements,
    pub(crate) subjects: Vec<usize>,
    pub(crate) facts: QueryFacts,
    pub(crate) services: QueryServices,
    pub(crate) warnings: Vec<ReadWarning>,
    pub(crate) upstream: Vec<super::super::candidates::QueryGlobalSelection>,
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "outcome",
    content = "response",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum QueryFactResult {
    Facts(Vec<QueryFactSelection>),
    Unavailable(SearchUnavailable),
}

fn required_graph<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

impl QueryFactSelection {
    pub(crate) fn covers(&self, requested: &FactRequirements) -> bool {
        self.covers_subject_selection(requested, None)
    }

    pub(crate) fn covers_original_subjects(
        &self,
        requested: &FactRequirements,
        subjects: &[usize],
    ) -> bool {
        if subjects
            .iter()
            .any(|subject| self.subjects.binary_search(subject).is_err())
        {
            return false;
        }
        self.covers_subject_selection(requested, Some(subjects))
    }

    fn covers_subject_selection(
        &self,
        requested: &FactRequirements,
        subjects: Option<&[usize]>,
    ) -> bool {
        use rift_search::query::FactRequirement;
        let original = &self.requirements;
        if !requested.required.iter().all(|required| {
            original.required.contains(required)
                || (*required == FactRequirement::Description
                    && original.requires(FactRequirement::CompleteDomain))
        }) || (subjects.is_none()
            && (original
                .symbol_name
                .as_ref()
                .is_some_and(|name| requested.symbol_name.as_ref() != Some(name))
                || original.symbol_ids.as_ref().is_some_and(|ids| {
                    requested
                        .symbol_ids
                        .as_ref()
                        .is_none_or(|selected| selected.iter().any(|id| !ids.contains(id)))
                })))
            || original.source_units.as_ref().is_some_and(|units| {
                requested
                    .source_units
                    .as_ref()
                    .is_none_or(|selected| selected.iter().any(|unit| !units.contains(unit)))
            })
            || (!requested.source_reads.is_empty()
                && (requested.source_reads != original.source_reads
                    || !self.source_records_complete()))
            || !facets_cover(&original.facets, &requested.facets)
        {
            return false;
        }
        let domain = super::complete_domain_adjacency(original);
        for requested in &requested.adjacency {
            let direct = original.adjacency.iter().any(|captured| {
                captured.seed == requested.seed
                    && captured.direction == requested.direction
                    && facets_cover(&captured.facets, &requested.facets)
                    && depth_covers(captured.depth, requested.depth)
            });
            if !(direct
                || (self.original_subject(&requested.seed)
                    && domain.iter().any(|(direction, facets, depth)| {
                        *direction == requested.direction
                            && facets_cover(facets, &requested.facets)
                            && depth_covers(*depth, requested.depth)
                    })))
            {
                return false;
            }
        }
        let all_occurrences = original.requires(FactRequirement::AllOccurrences)
            || (original.requires(FactRequirement::CompleteDomain)
                && original
                    .facets
                    .contains(&rift_protocol::read::RelationshipFacet::References));
        if requested.occurrences.iter().any(|seed| {
            !(original.occurrences.contains(seed)
                || (all_occurrences && self.original_subject(seed)))
        }) {
            return false;
        }
        if requested.history_symbols.iter().any(|seed| {
            !(original.history_symbols.contains(seed)
                || (original.requires(FactRequirement::AllHistories)
                    && self.original_subject(seed)))
        }) {
            return false;
        }
        true
    }

    fn source_records_complete(&self) -> bool {
        self.services.sources.len() == self.requirements.source_reads.len()
            && self
                .requirements
                .source_reads
                .iter()
                .zip(&self.services.sources)
                .all(|(request, (captured, result))| {
                    captured == request
                        && result.is_valid_for(request)
                        && matches!(
                            result,
                            rift_protocol::source_read::GetSourceResult::Found {
                                source_complete: true,
                                ..
                            } | rift_protocol::source_read::GetSourceResult::Missing { .. }
                        )
                })
    }

    fn original_subject(&self, seed: &rift_protocol::read::SymbolId) -> bool {
        self.subjects.iter().any(|index| {
            self.facts
                .symbols
                .get(*index)
                .is_some_and(|symbol| symbol.symbol.id.as_ref() == Some(seed))
        })
    }

    pub(crate) fn validate_coverage(&self, expected: &FactRequirements) -> Result<(), RiftError> {
        if &self.requirements != expected {
            return Err(acquisition_failure(
                "captured fact requirements differ from the original request",
            ));
        }
        self.requirements.validate_selection()?;
        self.requirements.validate_facts(&self.facts)?;
        if !self.source_records_complete() {
            return Err(acquisition_failure("required source result is unavailable"));
        }
        let mut previous = None;
        for &subject in &self.subjects {
            if subject >= self.facts.symbols.len() || previous.is_some_and(|index| index >= subject)
            {
                return Err(acquisition_failure(
                    "original subject selector is absent or repeats",
                ));
            }
            previous = Some(subject);
        }
        Ok(())
    }
}

pub(super) enum FactAcquisitions {
    Current(
        Vec<super::QueryAcquisition>,
        Vec<(
            QueryFactSelection,
            QueryServices,
            Option<rift_search::sparql::QueryIdentities>,
        )>,
    ),
    Captured(
        Vec<(
            QueryFactSelection,
            QueryServices,
            Option<rift_search::sparql::QueryIdentities>,
        )>,
    ),
}

pub(super) enum SelectedFactOwner {
    Local(rift_server::LocalSymbolViewInput, CapturedView),
    Query(rift_server::CapturedQueryView),
}

type SelectedFactView = Option<SelectedFactOwner>;

impl RiftMcp {
    pub(in crate::server) async fn query_facts_call(
        &self,
        request: rmcp::model::CallToolRequestParams,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        use rmcp::handler::server::tool::IntoCallToolResult as _;

        match self.authoritative_query_facts(request).await {
            Ok(response) => Ok(rmcp::model::CallToolResult::structured(response).into()),
            Err(failure) => failure.into_call_tool_result(),
        }
    }

    async fn authoritative_query_facts(
        &self,
        request: rmcp::model::CallToolRequestParams,
    ) -> Result<serde_json::Value, ToolFailure> {
        Box::pin(async move {
            let source_context = crate::global::candidates::SavedCandidateBackend::from_request(
                request.meta.as_ref(),
            )
            .map_err(failure)?;
            let deadline = self.request_deadline().await;
            let configuration = self.query_configuration().await;
            let limits = query_limits(&configuration)?;
            let budget = QueryBudget::for_request(limits, deadline.at().into_std(), &|| false)
                .map_err(failure)?;
            let request = self
                .decode_query_fact_request(request, &budget, deadline)
                .await?;
            if request
                .requirements
                .datasets
                .iter()
                .any(|dataset| dataset != LOCAL_GRAPH)
            {
                return self
                    .encode_query_fact_response(
                        QueryFactResult::Unavailable(unavailable(
                            SymbolUnavailableReason::ViewContext,
                        )),
                        &budget,
                        deadline,
                    )
                    .await;
            }
            let selected_view = match self.query_fact_view(request.view)? {
                Ok(selected) => selected,
                Err(reason) => {
                    return self
                        .encode_query_fact_response(
                            QueryFactResult::Unavailable(reason),
                            &budget,
                            deadline,
                        )
                        .await;
                }
            };
            let requests =
                rift_server::ProviderRequests::new(configuration.requests.min(request.requests));
            let request_maximum = requests.remaining();
            let selected = match self
                .acquire_fact_queries(
                    &request.requirements,
                    selected_view,
                    QueryExecution {
                        configuration: &configuration,
                        limits,
                        budget: &budget,
                        deadline,
                        requests: &requests,
                        source_context: source_context.as_ref(),
                    },
                    None,
                )
                .await?
            {
                Ok(selected) => selected,
                Err(reason) => {
                    return self
                        .encode_query_fact_response(
                            QueryFactResult::Unavailable(reason),
                            &budget,
                            deadline,
                        )
                        .await;
                }
            };
            self.capture_query_fact_response(
                selected,
                (request.requirements, request_maximum),
                requests,
                &budget,
                deadline,
            )
            .await
        })
        .await
    }

    pub(super) async fn acquire_fact_queries(
        &self,
        requirements: &FactRequirements,
        selected_view: SelectedFactView,
        execution: QueryExecution<'_>,
        plan: Option<&rift_search::sparql::QueryPlan>,
    ) -> Result<Result<FactAcquisitions, SearchUnavailable>, ToolFailure> {
        Box::pin(async move {
            let selected_view = match selected_view {
                Some(SelectedFactOwner::Query(captured)) => {
                    return self
                        .restore_fact_queries(requirements, Some(captured), execution, plan)
                        .await;
                }
                Some(SelectedFactOwner::Local(material, view)) => Some((material, view)),
                None => None,
            };
            if selected_view.is_some() && super::requires_engine(requirements) {
                return Ok(Err(unavailable(
                    SymbolUnavailableReason::InsufficientCoverage,
                )));
            }
            let mut retained = Vec::new();
            let mut current_requirements = None;
            if requirements
                .services
                .iter()
                .any(|service| service.starts_with(rift_search::sparql::RESULT_SERVICE_PREFIX))
            {
                if selected_view.is_some() {
                    return Ok(Err(unavailable(SymbolUnavailableReason::ViewContext)));
                }
                let restored = match self
                    .restore_fact_queries(requirements, None, execution, plan)
                    .await?
                {
                    Ok(FactAcquisitions::Captured(bodies)) => bodies,
                    Ok(FactAcquisitions::Current(_, _)) => {
                        return Err(failure(retention_failure()));
                    }
                    Err(reason) => return Ok(Err(reason)),
                };
                if requirements.retrievals.is_empty()
                    && requirements.datasets.is_empty()
                    && requirements.source_reads.is_empty()
                {
                    return Ok(Ok(FactAcquisitions::Captured(restored)));
                }
                let mut admission = execution.budget.clone();
                admission.charge(requirements).map_err(failure)?;
                let mut current = requirements.clone();
                current.services.retain(|service| {
                    !service.starts_with(rift_search::sparql::RESULT_SERVICE_PREFIX)
                });
                if current.retrievals.is_empty() && current.datasets.is_empty() {
                    current.required.clear();
                    current.symbol_name = None;
                    current.symbol_ids = None;
                    current.source_units = None;
                    current.occurrences.clear();
                    current.adjacency.clear();
                    current.history_symbols.clear();
                    current.facets.clear();
                }
                current_requirements = Some(current);
                retained = restored;
            }
            let requirements = current_requirements.as_ref().unwrap_or(requirements);
            self.acquire_current_fact_queries(requirements, selected_view, execution, retained)
                .await
        })
        .await
    }

    async fn acquire_current_fact_queries(
        &self,
        requirements: &FactRequirements,
        selected_view: Option<(rift_server::LocalSymbolViewInput, CapturedView)>,
        execution: QueryExecution<'_>,
        retained: Vec<(
            QueryFactSelection,
            QueryServices,
            Option<rift_search::sparql::QueryIdentities>,
        )>,
    ) -> Result<Result<FactAcquisitions, SearchUnavailable>, ToolFailure> {
        if requirements
            .retrievals
            .iter()
            .all(|request| request.change.is_none())
        {
            let selected = match self
                .acquire_query_data(
                    requirements,
                    &requirements.retrievals,
                    selected_view,
                    execution,
                )
                .await?
            {
                Ok(selected) => selected,
                Err(reason) => return Ok(Err(reason)),
            };
            let mut selected = selected;
            if (!requirements.retrievals.is_empty() || !requirements.source_reads.is_empty())
                && !requirements
                    .datasets
                    .iter()
                    .any(|graph| graph == LOCAL_GRAPH)
            {
                selected.graph = None;
            }
            let selected = self
                .query_work(execution.budget, execution.deadline, move |budget| {
                    selected.bind_targets(budget)
                })
                .await?;
            return Ok(Ok(FactAcquisitions::Current(vec![selected], retained)));
        }
        if selected_view.is_some() {
            return Ok(Err(unavailable(SymbolUnavailableReason::ViewContext)));
        }
        let selected = match self
            .acquire_change_fact_queries(requirements, execution)
            .await?
        {
            Ok(selected) => selected,
            Err(reason) => return Ok(Err(reason)),
        };
        Ok(Ok(FactAcquisitions::Current(selected, retained)))
    }

    async fn acquire_change_fact_queries(
        &self,
        requirements: &FactRequirements,
        execution: QueryExecution<'_>,
    ) -> Result<Result<Vec<super::QueryAcquisition>, SearchUnavailable>, ToolFailure> {
        let mut admission = execution.budget.clone();
        let count = requirements
            .retrievals
            .iter()
            .filter(|request| request.change.is_none())
            .count();
        admission
            .charge_material(
                count
                    .checked_mul(size_of::<rift_protocol::read::SearchParams>())
                    .ok_or_else(|| failure(retention_failure()))?,
            )
            .map_err(failure)?;
        let mut normal = Vec::new();
        normal
            .try_reserve_exact(count)
            .map_err(|_| failure(retention_failure()))?;
        for request in requirements
            .retrievals
            .iter()
            .filter(|request| request.change.is_none())
        {
            admission.charge(request).map_err(failure)?;
            normal.push(request.clone());
        }
        let mut selected = Vec::new();
        if !normal.is_empty() || !requirements.datasets.is_empty() {
            let body = match self
                .acquire_query_data(requirements, &normal, None, execution)
                .await?
            {
                Ok(body) => body,
                Err(reason) => return Ok(Err(reason)),
            };
            let mut body = body;
            if !requirements
                .datasets
                .iter()
                .any(|graph| graph == LOCAL_GRAPH)
            {
                body.graph = None;
            }
            let body = self
                .query_work(execution.budget, execution.deadline, move |budget| {
                    body.bind_targets(budget)
                })
                .await?;
            admission
                .charge_material(size_of::<super::QueryAcquisition>())
                .map_err(failure)?;
            selected
                .try_reserve_exact(1)
                .map_err(|_| failure(retention_failure()))?;
            selected.push(body);
        }
        for request in requirements
            .retrievals
            .iter()
            .filter(|request| request.change.is_some())
        {
            let mut bodies = match self
                .acquire_change_query(requirements, request, execution)
                .await?
            {
                Ok(bodies) => bodies,
                Err(reason) => return Ok(Err(reason)),
            };
            for body in &mut bodies {
                for service in &mut body.services.retrievals {
                    for (owner, _) in service.targets.iter_mut().flatten() {
                        *owner = owner.checked_add(selected.len()).ok_or_else(|| {
                            failure(acquisition_failure(
                                "query owner selector exceeds the platform bound",
                            ))
                        })?;
                    }
                }
            }
            admission
                .charge_material(
                    bodies
                        .len()
                        .checked_mul(size_of::<super::QueryAcquisition>())
                        .ok_or_else(|| failure(retention_failure()))?,
                )
                .map_err(failure)?;
            selected.try_reserve_exact(bodies.len()).map_err(|error| {
                failure(
                    errors::server::read_unavailable()
                        .operation("query acquisition")
                        .detail(error.to_string())
                        .error(),
                )
            })?;
            selected.append(&mut bodies);
        }
        Ok(Ok(selected))
    }

    pub(super) fn query_fact_view(
        &self,
        view: Option<CapturedViewId>,
    ) -> Result<Result<SelectedFactView, SearchUnavailable>, ToolFailure> {
        let Some(view) = view else {
            return Ok(Ok(None));
        };
        if let Some(captured) = self.captured_query_material(&view, "query facts")? {
            return Ok(Ok(Some(SelectedFactOwner::Query(captured))));
        }
        let selected = self.captured_context(&view, "query facts")?;
        Ok(match self.query_selection(&selected.view)? {
            Ok(Some(material)) => Ok(Some(SelectedFactOwner::Local(material, selected.view))),
            Ok(None) => Err(unavailable(SymbolUnavailableReason::ViewExpired)),
            Err(reason) => Err(unavailable(reason)),
        })
    }

    async fn decode_query_fact_request(
        &self,
        request: rmcp::model::CallToolRequestParams,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
    ) -> Result<QueryFactRequest, ToolFailure> {
        self.query_work(budget, deadline, move |budget| {
            let arguments = request.arguments.unwrap_or_default();
            budget.charge(&arguments)?;
            let request: QueryExchange<serde_json::Value> =
                serde_json::from_value(arguments.into()).map_err(|_| {
                    errors::server::read_invalid()
                        .field("arguments")
                        .violation("query fact arguments do not match the documented form")
                        .error()
                })?;
            let request = serde_json::from_value::<QueryFactRequest>(request.into_payload()?)
                .map_err(|_| {
                    errors::server::read_invalid()
                        .field("arguments")
                        .violation("query fact arguments do not match the documented form")
                        .error()
                })?;
            request.requirements.validate_selection()?;
            Ok(request)
        })
        .await
    }

    async fn capture_query_fact_response(
        &self,
        selected: FactAcquisitions,
        request: (FactRequirements, u64),
        requests: rift_server::ProviderRequests,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
    ) -> Result<serde_json::Value, ToolFailure> {
        let bodies = self
            .capture_query_fact_bodies(selected, request, requests, budget, deadline)
            .await?;
        let response = self
            .query_work(budget, deadline, move |budget| {
                let mut response = Vec::new();
                budget.charge_material(
                    bodies
                        .len()
                        .checked_mul(size_of::<QueryFactSelection>())
                        .ok_or_else(retention_failure)?,
                )?;
                response
                    .try_reserve_exact(bodies.len())
                    .map_err(|_| retention_failure())?;
                for (mut body, services, _) in bodies {
                    body.services = services;
                    response.push(body);
                }
                Ok(QueryFactResult::Facts(response))
            })
            .await?;
        self.encode_query_fact_response(response, budget, deadline)
            .await
    }

    pub(super) async fn capture_query_fact_bodies(
        &self,
        selected: FactAcquisitions,
        (requirements, request_maximum): (FactRequirements, u64),
        requests: rift_server::ProviderRequests,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
    ) -> Result<
        Vec<(
            QueryFactSelection,
            QueryServices,
            Option<rift_search::sparql::QueryIdentities>,
        )>,
        ToolFailure,
    > {
        let selected = match selected {
            FactAcquisitions::Captured(bodies) => return Ok(bodies),
            FactAcquisitions::Current(selected, retained) => (selected, retained),
        };
        let (selected, mut retained) = selected;
        let requirements = current_fact_requirements(requirements, !retained.is_empty());
        let views = std::sync::Arc::clone(&self.local_views);
        let responses = std::sync::Arc::clone(&self.candidate_responses);
        self.query_work(budget, deadline, move |budget| {
            let views = views.get().ok_or_else(retention_failure)?;
            if selected.is_empty() {
                return Err(retention_failure());
            }
            let mut bodies = Vec::new();
            let bytes = selected
                .len()
                .checked_mul(size_of::<QueryFactSelection>())
                .ok_or_else(retention_failure)?;
            budget.charge_material(bytes)?;
            bodies
                .try_reserve_exact(selected.len())
                .map_err(|_| retention_failure())?;
            let used = request_maximum.saturating_sub(requests.remaining());
            for (index, body) in selected.into_iter().enumerate() {
                requirements.validate_facts(&body.facts)?;
                let root = body.material.root().to_path_buf();
                let root_text = root.to_str().ok_or_else(retention_failure)?.to_owned();
                let owner = body.material.owner().clone();
                let selection = body.material.captured_provider_selection().ok_or_else(|| {
                    acquisition_failure("captured provider selection is unavailable")
                })?;
                budget.charge(selection)?;
                budget.charge(&requirements)?;
                let selection = selection.clone();
                let requirements = requirements.clone();
                let mut captured = None;
                let admitted = views.capture_with(body.material, |view| {
                    let selected = QueryFactSelection {
                        root: root_text,
                        owner: owner.clone(),
                        view: view.clone(),
                        selection,
                        requests: if index == 0 { used } else { 0 },
                        graph: body.graph,
                        requirements,
                        subjects: body.subjects,
                        facts: body.facts,
                        services: body.services,
                        warnings: body.warnings,
                        upstream: body.upstream,
                    };
                    selected.validate_coverage(&selected.requirements)?;
                    if !responses.save_symbol(&root, &owner, view)? {
                        return Ok(false);
                    }
                    captured = Some(selected);
                    Ok(true)
                })?;
                if admitted.is_none() {
                    return Err(retention_failure());
                }
                bodies.push(captured.ok_or_else(retention_failure)?);
            }
            attach_original_views(&mut bodies, budget)?;
            let mut selected = Vec::new();
            budget.charge_material(
                bodies
                    .len()
                    .checked_mul(size_of::<(
                        QueryFactSelection,
                        QueryServices,
                        Option<rift_search::sparql::QueryIdentities>,
                    )>())
                    .ok_or_else(retention_failure)?,
            )?;
            selected
                .try_reserve_exact(bodies.len())
                .map_err(|_| retention_failure())?;
            for body in bodies {
                budget.charge(&body.services)?;
                let services = body.services.clone();
                selected.push((body, services, None));
            }
            append_captured(&mut selected, &mut retained, budget)?;
            Ok(selected)
        })
        .await
    }

    async fn encode_query_fact_response(
        &self,
        response: QueryFactResult,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
    ) -> Result<serde_json::Value, ToolFailure> {
        self.query_work(budget, deadline, move |budget| {
            let response = QueryExchange::new(response);
            budget.charge(&response)?;
            let response = serde_json::to_value(response).map_err(encoding_failure)?;
            budget.check()?;
            Ok(response)
        })
        .await
    }
}

fn unavailable(reason: SymbolUnavailableReason) -> SearchUnavailable {
    SearchUnavailable::Unavailable {
        reason,
        view: None,
        warnings: Vec::new(),
    }
}

fn retention_failure() -> RiftError {
    acquisition_failure("query fact material could not be retained within its bound")
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}

fn attach_original_views(
    bodies: &mut [QueryFactSelection],
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    for index in 0..bodies.len() {
        let mut services = std::mem::take(&mut bodies[index].services);
        for service in &mut services.retrievals {
            if service.result.view.is_none() {
                budget.charge(&bodies[index].view)?;
                service.result.view = Some(bodies[index].view.clone());
            }
            if service.targets.len() != service.result.results.len() {
                return Err(acquisition_failure(
                    "retrieval record associations do not match selected rows",
                ));
            }
            for (hit, target) in service.result.results.iter_mut().zip(&service.targets) {
                if hit.view.is_none() {
                    let owner = target.as_ref().map_or(index, |(owner, _)| *owner);
                    let view = &bodies.get(owner).ok_or_else(retention_failure)?.view;
                    budget.charge(view)?;
                    hit.view = Some(view.clone());
                }
            }
        }
        bodies[index].services = services;
    }
    Ok(())
}

fn facets_cover(
    original: &[rift_protocol::read::RelationshipFacet],
    requested: &[rift_protocol::read::RelationshipFacet],
) -> bool {
    if original.is_empty() || requested.is_empty() {
        return original == requested;
    }
    requested.iter().all(|facet| original.contains(facet))
}

fn depth_covers(original: Option<u32>, requested: Option<u32>) -> bool {
    match (original, requested) {
        (None, _) => true,
        (Some(original), Some(requested)) => original >= requested,
        (Some(_), None) => false,
    }
}

fn append_captured(
    selected: &mut Vec<(
        QueryFactSelection,
        QueryServices,
        Option<rift_search::sparql::QueryIdentities>,
    )>,
    retained: &mut Vec<(
        QueryFactSelection,
        QueryServices,
        Option<rift_search::sparql::QueryIdentities>,
    )>,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    let offset = selected.len();
    for (body, services, _) in retained.iter_mut() {
        for services in [&mut body.services, services] {
            for service in &mut services.retrievals {
                for (owner, _) in service.targets.iter_mut().flatten() {
                    *owner = owner.checked_add(offset).ok_or_else(retention_failure)?;
                }
            }
        }
    }
    budget.charge_material(
        retained
            .len()
            .checked_mul(size_of::<(
                QueryFactSelection,
                QueryServices,
                Option<rift_search::sparql::QueryIdentities>,
            )>())
            .ok_or_else(retention_failure)?,
    )?;
    selected
        .try_reserve_exact(retained.len())
        .map_err(|_| retention_failure())?;
    selected.append(retained);
    Ok(())
}

fn current_fact_requirements(
    mut requirements: FactRequirements,
    has_retained: bool,
) -> FactRequirements {
    if has_retained
        && requirements.retrievals.is_empty()
        && requirements.datasets.is_empty()
        && !requirements.source_reads.is_empty()
    {
        requirements.required.clear();
        requirements.symbol_name = None;
        requirements.symbol_ids = None;
        requirements.source_units = None;
        requirements.occurrences.clear();
        requirements.adjacency.clear();
        requirements.history_symbols.clear();
        requirements.facets.clear();
        requirements
            .services
            .retain(|service| !service.starts_with(rift_search::sparql::RESULT_SERVICE_PREFIX));
    }
    requirements
}
