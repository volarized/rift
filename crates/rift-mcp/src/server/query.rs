//! Query acquisition and execution under the existing publication and worker owners.

use rift_search::query::FactRequirement;
use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use rift_error::{RiftError, errors};
use rift_protocol::query::{QueryConfiguration, QueryParams, QueryReadResult};
use rift_protocol::read::{ReadWarning, TraversalDirection};
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::SymbolUnavailableReason;
use rift_search::sparql::{
    FactRequirements, LOCAL_GRAPH, QueryFacts, QueryLimits, QueryPlan, QueryServiceData,
    QueryServices, RESULT_SERVICE_PREFIX, RetainedResult,
};

use super::{Json, RiftMcp, ToolFailure, wire};
use crate::failure::McpErrorExt as _;

mod facts;
pub(crate) use facts::{QueryFactRequest, QueryFactResult, QueryFactSelection};
mod global;
mod retrieval;
mod selection;

type QueryBudget = rift_server::QueryFactBudget<'static>;

#[derive(Clone, Copy)]
struct QueryExecution<'a> {
    configuration: &'a QueryConfiguration,
    limits: QueryLimits,
    budget: &'a QueryBudget,
    deadline: super::RequestDeadline,
    requests: &'a rift_server::ProviderRequests,
}

type QueryProjection = (
    (
        QueryFacts,
        rift_server::LocalSymbolViewInput,
        rift_search::query::QueryProviderSelection,
    ),
    Vec<ReadWarning>,
);

impl RiftMcp {
    async fn query_configuration(&self) -> QueryConfiguration {
        self.published
            .read()
            .await
            .current
            .configuration
            .search_configuration()
            .query
    }

    pub(super) fn query_candidates_call(
        _request: rmcp::model::CallToolRequestParams,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        use rmcp::handler::server::tool::IntoCallToolResult;
        failure(
            errors::server::read_unsupported()
                .capability("query evaluation does not accept supplied fact records")
                .error(),
        )
        .into_call_tool_result()
    }

    fn query_program(&self) -> Result<Arc<std::path::PathBuf>, ToolFailure> {
        self.query_program.clone().ok_or_else(|| {
            failure(
                errors::server::read_unsupported()
                    .capability("query execution requires a compatible Rift executable")
                    .error(),
            )
        })
    }

    pub(super) async fn graph_query(
        &self,
        params: QueryParams,
    ) -> Result<Json<QueryReadResult>, ToolFailure> {
        Box::pin(async move {
            let deadline = self.request_deadline().await;
            let configuration = self.query_configuration().await;
            let limits = query_limits(&configuration)?;
            let budget = QueryBudget::for_request(limits, deadline.at().into_std(), &|| false)
                .map_err(failure)?;
            let program = self.query_program()?;
            let cancellation = tokio_util::sync::CancellationToken::new();
            let guard = cancellation.clone().drop_guard();
            let prepared = tokio::time::timeout_at(
                deadline.at(),
                self.blocking.run_with_cancellation(
                    "query parse",
                    cancellation,
                    move |cancelled| {
                        rift_server::prepare_query(
                            program.as_path(),
                            params.query,
                            limits,
                            deadline.at().into_std(),
                            &|| cancelled.is_cancelled(),
                        )
                    },
                ),
            )
            .await
            .map_err(|_| {
                failure(acquisition_failure(
                    "query parsing exceeded the request deadline",
                ))
            })?
            .map_err(failure)?;
            drop(guard);
            let prepared = self.admit_query_data(&budget, prepared, deadline).await?;
            if prepared
                .requirements()
                .datasets
                .iter()
                .any(|dataset| dataset != LOCAL_GRAPH)
            {
                return Ok(Json(QueryReadResult::Unavailable(
                    SearchUnavailable::Unavailable {
                        reason: SymbolUnavailableReason::ViewContext,
                        view: None,
                        warnings: Vec::new(),
                    },
                )));
            }
            if let Some(selection) = prepared.page_selection()
                && let Some(result) = self.retained_query_page(selection).await?
            {
                return Ok(Json(result));
            }
            let requests = rift_server::ProviderRequests::new(configuration.requests);
            let execution = QueryExecution {
                configuration: &configuration,
                limits,
                budget: &budget,
                deadline,
                requests: &requests,
            };
            let selected = match self
                .acquire_query_data(
                    prepared.requirements(),
                    prepared.retrievals(),
                    None,
                    execution,
                )
                .await?
            {
                Ok(selected) => selected,
                Err(reason) => return Ok(Json(QueryReadResult::Unavailable(reason))),
            };
            let selected = self
                .query_work(&budget, deadline, move |budget| {
                    selected.bind_targets(budget)
                })
                .await?;
            let answer = self
                .evaluate_query(
                    prepared,
                    selected.facts,
                    limits,
                    selected.services,
                    deadline,
                )
                .await?;
            self.retain_query(
                selected.material,
                answer,
                &configuration,
                selected.warnings,
                selected.upstream,
            )
            .await
            .map(Json)
        })
        .await
    }

    async fn acquire_query_data(
        &self,
        requirements: &FactRequirements,
        retrievals: &[rift_protocol::read::SearchParams],
        selected_view: Option<(
            rift_server::LocalSymbolViewInput,
            rift_protocol::symbol_read::CapturedView,
        )>,
        execution: QueryExecution<'_>,
    ) -> Result<Result<QueryAcquisition, SearchUnavailable>, ToolFailure> {
        let mut acquisition = requirements.clone();
        if retrievals.iter().any(|request| {
            matches!(
                request.target,
                rift_protocol::read::SearchParamsTarget::All
                    | rift_protocol::read::SearchParamsTarget::Symbol
            )
        }) {
            acquisition.require(FactRequirement::Domain);
            acquisition.require(FactRequirement::Description);
        }
        let requirements = &acquisition;
        let mut selected = match self
            .selected_query_services(requirements, retrievals, selected_view, execution)
            .await?
        {
            Ok(selected) => selected,
            Err(reason) => return Ok(Err(reason)),
        };
        let requires_semantic = requires_engine(requirements);
        if !requirements.requires(FactRequirement::Domain) {
            let material = self
                .query_selection_material(selected.material.take(), selected.expiry.take())
                .await?;
            return Ok(Ok(
                selected.into_acquisition(QueryFacts::default(), material)
            ));
        }
        if !requires_semantic && let Some(material) = selected.material.take() {
            let material = limit_material(material, selected.expiry.as_ref())?;
            let (facts, material) = self
                .project_retained_query(material, requirements, execution)
                .await?;
            return Ok(Ok(selected.into_acquisition(facts, material)));
        }
        self.acquire_current_query(requirements, selected, requires_semantic, execution)
            .await
    }

    async fn acquire_current_query(
        &self,
        requirements: &FactRequirements,
        selected: SelectedQueryServices,
        requires_semantic: bool,
        execution: QueryExecution<'_>,
    ) -> Result<Result<QueryAcquisition, SearchUnavailable>, ToolFailure> {
        let QueryExecution { deadline, .. } = execution;
        let SelectedQueryServices {
            services,
            contributions,
            material: retained_material,
            warnings: retained_warnings,
            expiry,
            mut upstream,
        } = selected;
        let resolved = match self.query_workspace(execution).await? {
            Ok(resolved) => resolved,
            Err(reason) => return Ok(Err(reason)),
        };
        let accepted = resolved
            .published
            .configuration
            .accepted_for_tool(wire::ErrorPhase::Read)?;
        let ((facts, material, provider_selection), mut warnings) = self
            .project_fresh_query(&resolved, requirements, &accepted, execution)
            .await?;
        let material = self
            .combine_query_material(material, retained_material, execution)
            .await?;
        let material = limit_material(material, expiry.as_ref())?;
        warnings.extend(retained_warnings);
        let (references, global_selection, global_warnings) = if requires_semantic {
            let (engines, roots) = self
                .query_engine_roots(&resolved, &accepted, deadline)
                .await?;
            self.acquire_query(
                ScopedQueryInput {
                    resolved: &resolved,
                    engines: &engines,
                    roots: &roots,
                    accepted: &accepted,
                    execution,
                },
                requirements,
                &facts,
                &upstream,
                provider_selection,
            )
            .await
            .map_err(failure)?
        } else {
            (rift_server::EngineReferences::default(), None, Vec::new())
        };
        warnings.extend(global_warnings);
        if let Some(selected) = &global_selection
            && let Err(reason) = selection::include(&mut upstream, selected.clone())
        {
            return Ok(Err(SearchUnavailable::Unavailable {
                reason,
                view: None,
                warnings,
            }));
        }
        warnings.extend(references.warnings());
        if !references.is_complete()
            || (requires_semantic && !self.engine_tree_matches(&resolved.published).await?)
        {
            return Ok(Err(SearchUnavailable::Unavailable {
                reason: SymbolUnavailableReason::InsufficientCoverage,
                view: None,
                warnings,
            }));
        }
        let material = limit_material(
            material,
            global_selection.as_ref().map(|selected| &selected.view),
        )?;
        let (facts, material) = self
            .attach_query_references(facts, material, references, execution)
            .await?;
        Ok(Ok(QueryAcquisition {
            facts,
            material,
            services,
            contributions,
            warnings,
            upstream,
        }))
    }

    async fn query_workspace(
        &self,
        execution: QueryExecution<'_>,
    ) -> Result<Result<super::ResolvedWorkspace, SearchUnavailable>, ToolFailure> {
        let QueryExecution {
            configuration,
            deadline,
            ..
        } = execution;
        let resolved = self
            .published_workspace(wire::ErrorPhase::Read, deadline)
            .await?;
        if resolved
            .published
            .configuration
            .search_configuration()
            .query
            != *configuration
        {
            return Err(failure(acquisition_failure(
                "query configuration changed during parsing; resend the same request",
            )));
        }
        if resolved.published.preparation.is_some() {
            return Ok(Err(SearchUnavailable::Unavailable {
                reason: SymbolUnavailableReason::IndexPreparing,
                view: None,
                warnings: resolved.stale.into_iter().collect(),
            }));
        }
        Ok(Ok(resolved))
    }

    async fn combine_query_material(
        &self,
        material: rift_server::LocalSymbolViewInput,
        retained_material: Option<rift_server::LocalSymbolViewInput>,
        execution: QueryExecution<'_>,
    ) -> Result<rift_server::LocalSymbolViewInput, ToolFailure> {
        let QueryExecution {
            budget, deadline, ..
        } = execution;
        let material = match retained_material {
            Some(selected) => self.query_work(budget, deadline, move |budget| {
                for input in [&material, &selected] {
                    if let Some(references) = input.query_references() {
                        budget.charge_material(references.retained_material_bytes())?;
                    }
                }
                for facts in [material.package_facts(), selected.package_facts()].into_iter().flatten() {
                    budget.charge(facts.as_ref())?;
                }
                material.combine(&selected).map_err(|_| acquisition_failure("selected query facts belong to different publications; select one retained context"))
            }).await?,
            None => material,
        };
        Ok(material)
    }

    async fn selected_query_services(
        &self,
        requirements: &FactRequirements,
        retrievals: &[rift_protocol::read::SearchParams],
        selected_view: Option<(
            rift_server::LocalSymbolViewInput,
            rift_protocol::symbol_read::CapturedView,
        )>,
        execution: QueryExecution<'_>,
    ) -> Result<Result<SelectedQueryServices, SearchUnavailable>, ToolFailure> {
        let QueryExecution {
            budget, deadline, ..
        } = execution;
        let mut selected = match self
            .query_services(requirements, retrievals, execution)
            .await?
        {
            Ok(selected) => selected,
            Err(reason) => return Ok(Err(reason)),
        };
        if let Some((material, view)) = selected_view {
            selected.include_view(&view)?;
            let (updated, admitted) = self
                .include_query_material(selected, material, budget, deadline)
                .await?;
            selected = updated;
            if let Err(reason) = admitted {
                return Ok(Err(SearchUnavailable::Unavailable {
                    reason,
                    view: Some(view),
                    warnings: selected.warnings,
                }));
            }
        }
        Ok(Ok(selected))
    }

    async fn query_selection_material(
        &self,
        retained_material: Option<rift_server::LocalSymbolViewInput>,
        expiry: Option<rift_protocol::symbol_read::CapturedView>,
    ) -> Result<rift_server::LocalSymbolViewInput, ToolFailure> {
        let material = if let Some(material) = retained_material {
            material
        } else {
            let published = Arc::clone(&self.published.read().await.current);
            let reads = Arc::clone(&published.reads);
            let prepared = published.preparation.is_none();
            let accepted = published
                .configuration
                .accepted_for_tool(wire::ErrorPhase::Read)?;
            self.blocking
                .run("query selection", move || {
                    let material = reads.local_search_view_input(prepared)?;
                    let selection = material.provider_selection(&accepted)?;
                    material.with_provider_selection(selection)
                })
                .await
                .map_err(failure)?
        };
        let material = limit_material(material, expiry.as_ref())?;
        Ok(material)
    }

    async fn project_retained_query(
        &self,
        material: rift_server::LocalSymbolViewInput,
        requirements: &FactRequirements,
        execution: QueryExecution<'_>,
    ) -> Result<(QueryFacts, rift_server::LocalSymbolViewInput), ToolFailure> {
        let QueryExecution {
            limits,
            budget,
            deadline,
            ..
        } = execution;
        let requirements = requirements.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let projection_budget = budget.clone();
        let projection_configuration = self
            .published
            .read()
            .await
            .current
            .configuration
            .accepted_for_tool(wire::ErrorPhase::Read)?;
        let (facts, material) = tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run_with_cancellation(
                "retained query facts",
                cancellation,
                move |cancelled| {
                    let cancelled = || cancelled.is_cancelled();
                    let mut budget = projection_budget.with_cancellation(&cancelled);
                    let selection = material.captured_provider_selection().ok_or_else(|| {
                        acquisition_failure("captured provider selection is unavailable")
                    })?;
                    if selection.configuration
                        != rift_search::query::QueryProviderSelection::configuration_digest(
                            &projection_configuration,
                        )?
                    {
                        return Err(acquisition_failure(
                            "captured provider configuration does not match",
                        ));
                    }
                    Ok((
                        material.query_facts_with_selection(
                            &requirements,
                            limits,
                            &mut budget,
                            selection,
                        )?,
                        material,
                    ))
                },
            ),
        )
        .await
        .map_err(|_| {
            failure(acquisition_failure(
                "query acquisition exceeded its deadline",
            ))
        })?
        .map_err(failure)?;
        drop(guard);
        Ok((facts, material))
    }

    async fn project_fresh_query(
        &self,
        resolved: &super::ResolvedWorkspace,
        requirements: &FactRequirements,
        accepted: &rift_protocol::configuration::WorkspaceConfiguration,
        execution: QueryExecution<'_>,
    ) -> Result<QueryProjection, ToolFailure> {
        let QueryExecution {
            limits,
            budget,
            deadline,
            ..
        } = execution;
        let reads = Arc::clone(&resolved.published.reads);
        if let Some(history) = &self.history {
            reads.attach_history_store(history.stored());
        }
        let projection_requirements = requirements.clone();
        let projection_configuration = accepted.clone();
        let projection = {
            let cancellation = tokio_util::sync::CancellationToken::new();
            let guard = cancellation.clone().drop_guard();
            let projection_budget = budget.clone();
            let projection = tokio::time::timeout_at(
                deadline.at(),
                self.blocking.run_with_cancellation(
                    "query facts",
                    cancellation,
                    move |cancelled| {
                        let cancelled = || cancelled.is_cancelled();
                        let mut budget = projection_budget.with_cancellation(&cancelled);
                        let material = reads.local_search_view_input(true)?;
                        let selection = material.provider_selection(&projection_configuration)?;
                        budget.charge(&selection)?;
                        let material = material.with_provider_selection(selection.clone())?;
                        let mut facts = material.query_facts_with_selection(
                            &projection_requirements,
                            limits,
                            &mut budget,
                            &selection,
                        )?;
                        reads.query_histories(
                            &material,
                            &projection_requirements,
                            &mut facts,
                            limits,
                            &mut budget,
                        )?;
                        let complete_catalog = projection_requirements
                            .requires(FactRequirement::AllHistories)
                            && !facts
                                .unavailable_families
                                .contains(&rift_search::query::QueryFactFamily::History);
                        let material = material.with_histories(
                            &facts.histories,
                            complete_catalog,
                            limits,
                            &mut budget,
                        )?;
                        Ok((facts, material, selection))
                    },
                ),
            )
            .await
            .map_err(|_| {
                failure(acquisition_failure(
                    "query acquisition exceeded its deadline",
                ))
            })?
            .map_err(failure)?;
            drop(guard);
            (
                projection,
                resolved.stale.clone().into_iter().collect::<Vec<_>>(),
            )
        };
        Ok(projection)
    }

    async fn query_engine_roots(
        &self,
        resolved: &super::ResolvedWorkspace,
        accepted: &rift_protocol::configuration::WorkspaceConfiguration,
        deadline: super::RequestDeadline,
    ) -> Result<(Arc<rift_server::EnginePool>, rift_server::CalleeRoots), ToolFailure> {
        let engines = self.engine_pool_for(&resolved.published).await;
        let roots_reads = Arc::clone(&resolved.published.reads);
        let roots_engines = Arc::clone(&engines);
        let roots_configuration = accepted.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let roots = tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run_with_cancellation(
                "query package roots",
                cancellation,
                move |cancelled| {
                    rift_server::CalleeRoots::read_query(
                        &roots_reads,
                        &roots_engines,
                        &roots_configuration,
                        deadline.at(),
                        &|| cancelled.is_cancelled(),
                    )
                },
            ),
        )
        .await
        .map_err(|_| {
            failure(acquisition_failure(
                "query package roots exceeded the request deadline",
            ))
        })?
        .map_err(failure)?;
        drop(guard);
        Ok((engines, roots))
    }

    async fn attach_query_references(
        &self,
        mut facts: QueryFacts,
        material: rift_server::LocalSymbolViewInput,
        references: rift_server::EngineReferences,
        execution: QueryExecution<'_>,
    ) -> Result<(QueryFacts, rift_server::LocalSymbolViewInput), ToolFailure> {
        let QueryExecution {
            budget, deadline, ..
        } = execution;
        self.query_work(budget, deadline, move |budget| {
            for relationship in references.query_local_relationships() {
                budget.charge(relationship)?;
                facts.relationships.push(relationship.clone());
            }
            for occurrence in references.query_occurrences() {
                budget.charge(occurrence)?;
                facts.occurrences.push(occurrence.clone());
            }
            let package_facts = references.query_package_facts();
            budget.charge(package_facts)?;
            rift_server::merge_query_facts(&mut facts, package_facts.clone())?;
            references.project_provider_edges(&mut facts)?;
            budget.charge(package_facts)?;
            if let Some(existing) = material.package_facts() {
                budget.charge(existing.as_ref())?;
            }
            let material = material.with_package_facts(package_facts.clone())?;
            budget.charge_material(references.retained_material_bytes())?;
            if let Some(existing) = material.query_references() {
                budget.charge_material(existing.retained_material_bytes())?;
            }
            let material = material.with_query_references(references)?;
            Ok((facts, material))
        })
        .await
    }

    async fn evaluate_query(
        &self,
        prepared: QueryPlan,
        facts: QueryFacts,
        limits: QueryLimits,
        services: QueryServices,
        deadline: super::RequestDeadline,
    ) -> Result<rift_protocol::query::QueryAnswer, ToolFailure> {
        let program = self.query_program.clone().ok_or_else(|| {
            failure(
                errors::server::read_unsupported()
                    .capability("query execution requires a compatible Rift executable")
                    .error(),
            )
        })?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let answer = tokio::time::timeout_at(
            deadline.at(),
            self.blocking
                .run_with_cancellation("query evaluate", cancellation, move |cancelled| {
                    rift_server::evaluate_query(
                        program.as_path(),
                        prepared,
                        facts,
                        limits,
                        services,
                        deadline.at().into_std(),
                        &|| cancelled.is_cancelled(),
                    )
                }),
        )
        .await
        .map_err(|_| {
            failure(
                errors::server::read_unavailable()
                    .operation("query evaluation")
                    .detail("request deadline exceeded; narrow the query and retry")
                    .error(),
            )
        })?
        .map_err(failure)?;
        drop(guard);
        Ok(answer)
    }

    async fn query_work<T: Send + 'static>(
        &self,
        budget: &QueryBudget,
        deadline: super::RequestDeadline,
        work: impl FnOnce(&mut rift_server::QueryFactBudget<'_>) -> Result<T, RiftError>
        + Send
        + 'static,
    ) -> Result<T, ToolFailure> {
        let budget = budget.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let result = tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run_with_cancellation(
                "query acquisition",
                cancellation,
                move |cancelled| {
                    let cancelled = || cancelled.is_cancelled();
                    let mut budget = budget.with_cancellation(&cancelled);
                    budget.check()?;
                    let value = work(&mut budget)?;
                    budget.check()?;
                    Ok(value)
                },
            ),
        )
        .await
        .map_err(|_| {
            failure(acquisition_failure(
                "query acquisition exceeded its deadline",
            ))
        })?
        .map_err(failure)?;
        drop(guard);
        Ok(result)
    }

    async fn admit_query_data<T: serde::Serialize + Send + 'static>(
        &self,
        budget: &QueryBudget,
        value: T,
        deadline: super::RequestDeadline,
    ) -> Result<T, ToolFailure> {
        self.query_work(budget, deadline, move |budget| {
            budget.charge(&value)?;
            Ok(value)
        })
        .await
    }

    async fn include_query_material(
        &self,
        mut selected: SelectedQueryServices,
        material: rift_server::LocalSymbolViewInput,
        budget: &QueryBudget,
        deadline: super::RequestDeadline,
    ) -> Result<(SelectedQueryServices, Result<(), SymbolUnavailableReason>), ToolFailure> {
        self.query_work(budget, deadline, move |budget| {
            if selected.material.is_some() {
                for facts in [
                    material.package_facts(),
                    selected
                        .material
                        .as_ref()
                        .and_then(rift_server::LocalSymbolViewInput::package_facts),
                ]
                .into_iter()
                .flatten()
                {
                    budget.charge(facts.as_ref())?;
                }
            }
            let admitted = selected.include_material(material);
            Ok((selected, admitted))
        })
        .await
    }

    async fn query_services(
        &self,
        requirements: &FactRequirements,
        retrievals: &[rift_protocol::read::SearchParams],
        execution: QueryExecution<'_>,
    ) -> Result<Result<SelectedQueryServices, SearchUnavailable>, ToolFailure> {
        let QueryExecution {
            configuration,
            budget,
            deadline,
            requests,
            ..
        } = execution;
        let mut selected = SelectedQueryServices::default();
        let global_configuration = self
            .published
            .read()
            .await
            .current
            .configuration
            .global_configuration();
        for request in retrievals {
            let (result, contributions) = tokio::time::timeout_at(
                deadline.at(),
                self.complete_retrieval(
                    request,
                    configuration,
                    requests,
                    budget,
                    deadline,
                    &mut selected,
                ),
            )
            .await
            .map_err(|_| {
                failure(acquisition_failure(
                    "retrieval acquisition exceeded the request deadline",
                ))
            })??;
            selected = match self
                .include_retrieval_result(
                    selected,
                    request,
                    result,
                    contributions,
                    &global_configuration,
                    execution,
                )
                .await?
            {
                Ok(selected) => selected,
                Err(reason) => return Ok(Err(reason)),
            };
        }
        selected = match self
            .include_query_bindings(selected, &requirements.services, execution)
            .await?
        {
            Ok(selected) => selected,
            Err(reason) => return Ok(Err(reason)),
        };
        if self
            .published
            .read()
            .await
            .current
            .configuration
            .global_configuration()
            != global_configuration
        {
            return Err(failure(acquisition_failure(
                "global configuration changed during query acquisition",
            )));
        }
        Ok(Ok(selected))
    }
    async fn include_retrieval_result(
        &self,
        mut selected: SelectedQueryServices,
        request: &rift_protocol::read::SearchParams,
        result: rift_protocol::search_read::SearchReadResult,
        contributions: Vec<Vec<rift_core::ContributionKey>>,
        global_configuration: &rift_protocol::configuration::GlobalConfiguration,
        execution: QueryExecution<'_>,
    ) -> Result<Result<SelectedQueryServices, SearchUnavailable>, ToolFailure> {
        let QueryExecution {
            budget, deadline, ..
        } = execution;
        match result {
            rift_protocol::search_read::SearchReadResult::Results(result) => {
                let result = self.admit_query_data(budget, result, deadline).await?;
                if result.warnings.iter().any(incomplete_retrieval) {
                    return Ok(Err(SearchUnavailable::Unavailable {
                        reason: SymbolUnavailableReason::InsufficientCoverage,
                        view: result.view,
                        warnings: result.warnings,
                    }));
                }
                selected.warnings.extend(result.warnings.iter().cloned());
                if let Some(view) = &result.view {
                    selected.include_view(view)?;
                    match self.query_selection(view)? {
                        Ok(Some(material)) => {
                            let (updated, admitted) = self
                                .include_query_material(selected, material, budget, deadline)
                                .await?;
                            selected = updated;
                            if let Err(reason) = admitted {
                                return Ok(Err(SearchUnavailable::Unavailable {
                                    reason,
                                    view: Some(view.clone()),
                                    warnings: selected.warnings,
                                }));
                            }
                        }
                        Ok(None) => {}
                        Err(reason) => {
                            return Ok(Err(SearchUnavailable::Unavailable {
                                reason,
                                view: Some(view.clone()),
                                warnings: selected.warnings,
                            }));
                        }
                    }
                }
                for view in result.results.iter().filter_map(|hit| hit.view.as_ref()) {
                    selected.include_view(view)?;
                }
                for hit in &result.results {
                    if let Some(upstream) =
                        selection::hit_selection(hit, global_configuration).map_err(failure)?
                        && let Err(reason) = selection::include(&mut selected.upstream, upstream)
                    {
                        return Ok(Err(SearchUnavailable::Unavailable {
                            reason,
                            view: hit.view.clone(),
                            warnings: selected.warnings,
                        }));
                    }
                }
                selected.services.retrievals.push(QueryServiceData {
                    request: request.clone(),
                    result: *result,
                    targets: Vec::new(),
                });
                selected.contributions.push(contributions);
            }
            rift_protocol::search_read::SearchReadResult::Unavailable(reason) => {
                return Ok(Err(reason));
            }
        }
        Ok(Ok(selected))
    }

    async fn include_query_bindings(
        &self,
        mut selected: SelectedQueryServices,
        services: &[String],
        execution: QueryExecution<'_>,
    ) -> Result<Result<SelectedQueryServices, SearchUnavailable>, ToolFailure> {
        let QueryExecution {
            budget, deadline, ..
        } = execution;
        for service in services {
            let Some(id) = service.strip_prefix(RESULT_SERVICE_PREFIX) else {
                continue;
            };
            let (id, page) = match id.rsplit_once("?page=") {
                Some((id, page)) => (
                    id,
                    Some(page.parse::<u64>().map_err(|_| {
                        failure(acquisition_failure("retained query page is invalid"))
                    })?),
                ),
                None => (id, None),
            };
            let view = rift_protocol::symbol_read::CapturedViewId::parse(id).map_err(failure)?;
            let retained = match self
                .retained_query(&view, page, budget.clone(), deadline)
                .await?
            {
                Ok(retained) => retained,
                Err(reason) => {
                    return Ok(Err(SearchUnavailable::Unavailable {
                        reason,
                        view: None,
                        warnings: Vec::new(),
                    }));
                }
            };
            let (updated, admitted) = self
                .include_query_material(selected, retained.material, budget, deadline)
                .await?;
            selected = updated;
            if let Err(reason) = admitted {
                return Ok(Err(SearchUnavailable::Unavailable {
                    reason,
                    view: None,
                    warnings: selected.warnings,
                }));
            }
            selected.warnings.extend(retained.warnings);
            for upstream in retained.upstream {
                if let Err(reason) = selection::include(&mut selected.upstream, upstream) {
                    return Ok(Err(SearchUnavailable::Unavailable {
                        reason,
                        view: None,
                        warnings: selected.warnings,
                    }));
                }
            }
            selected.services.bindings.push(RetainedResult {
                service: service.clone(),
                answer: retained.answer,
            });
        }
        Ok(Ok(selected))
    }
}

fn requires_engine(requirements: &FactRequirements) -> bool {
    requirements.requires(FactRequirement::CompleteDomain)
        || requirements.requires(FactRequirement::AllOccurrences)
        || !requirements.occurrences.is_empty()
        || !requirements.adjacency.is_empty()
}

struct QueryAcquisition {
    facts: QueryFacts,
    material: rift_server::LocalSymbolViewInput,
    services: QueryServices,
    contributions: Vec<Vec<Vec<rift_core::ContributionKey>>>,
    warnings: Vec<ReadWarning>,
    upstream: Vec<super::candidates::QueryGlobalSelection>,
}

#[derive(Default)]
struct SelectedQueryServices {
    services: QueryServices,
    contributions: Vec<Vec<Vec<rift_core::ContributionKey>>>,
    material: Option<rift_server::LocalSymbolViewInput>,
    warnings: Vec<rift_protocol::read::ReadWarning>,
    expiry: Option<rift_protocol::symbol_read::CapturedView>,
    upstream: Vec<super::candidates::QueryGlobalSelection>,
}

impl SelectedQueryServices {
    fn into_acquisition(
        self,
        facts: QueryFacts,
        material: rift_server::LocalSymbolViewInput,
    ) -> QueryAcquisition {
        QueryAcquisition {
            facts,
            material,
            services: self.services,
            contributions: self.contributions,
            warnings: self.warnings,
            upstream: self.upstream,
        }
    }

    fn include_material(
        &mut self,
        material: rift_server::LocalSymbolViewInput,
    ) -> Result<(), SymbolUnavailableReason> {
        self.material = Some(match self.material.take() {
            Some(selected) => selected.combine(&material)?,
            None => material,
        });
        Ok(())
    }

    fn include_view(
        &mut self,
        view: &rift_protocol::symbol_read::CapturedView,
    ) -> Result<(), ToolFailure> {
        let expires = crate::proxy::captured_expiry(view).map_err(failure)?;
        let earlier = match &self.expiry {
            Some(held) => expires < crate::proxy::captured_expiry(held).map_err(failure)?,
            None => true,
        };
        if earlier {
            self.expiry = Some(view.clone());
        }
        Ok(())
    }
}

fn incomplete_retrieval(warning: &ReadWarning) -> bool {
    matches!(
        warning,
        ReadWarning::LocalIndexPreparing { .. }
            | ReadWarning::VectorIndexPreparing { .. }
            | ReadWarning::LexicalRankingUnavailable { .. }
            | ReadWarning::LexicalRankingTruncated { .. }
            | ReadWarning::QueryNarrowed { .. }
            | ReadWarning::ResultsTruncated { .. }
            | ReadWarning::PatternMatchesTruncated { .. }
            | ReadWarning::PatternIndexPreparing { .. }
            | ReadWarning::EngineAnalysisUnavailable { .. }
            | ReadWarning::RelationshipCoverageMissing { .. }
            | ReadWarning::TraversalTruncated { .. }
            | ReadWarning::CalleesDropped { .. }
            | ReadWarning::HistoryStoreFilling { .. }
            | ReadWarning::ChangeTruncated { .. }
            | ReadWarning::GlobalAccessDisabled
            | ReadWarning::GlobalApiUnavailable { .. }
            | ReadWarning::GlobalPublicationIncompatible { .. }
            | ReadWarning::GlobalResponseInvalid { .. }
            | ReadWarning::GlobalPageWarning { .. }
            | ReadWarning::PackageAbsent { .. }
            | ReadWarning::PackageRequirementAbsent { .. }
            | ReadWarning::PackageSubstituted { .. }
            | ReadWarning::PackageContextDegraded { .. }
    )
}

fn limit_material(
    material: rift_server::LocalSymbolViewInput,
    expiry: Option<&rift_protocol::symbol_read::CapturedView>,
) -> Result<rift_server::LocalSymbolViewInput, ToolFailure> {
    match expiry {
        Some(expiry) => material.limited_by(expiry).map_err(failure),
        None => Ok(material),
    }
}

#[derive(Clone, Copy)]
struct ScopedQueryInput<'query> {
    resolved: &'query super::ResolvedWorkspace,
    engines: &'query rift_server::EnginePool,
    roots: &'query rift_server::CalleeRoots,
    accepted: &'query rift_protocol::configuration::WorkspaceConfiguration,
    execution: QueryExecution<'query>,
}

fn canonical_query_symbols(facts: &QueryFacts) -> Vec<rift_protocol::read::SymbolId> {
    facts
        .symbols
        .iter()
        .filter_map(|item| item.symbol.id.clone())
        .collect()
}

fn occurrence_subjects(
    requirements: &FactRequirements,
    facts: &QueryFacts,
) -> Vec<rift_protocol::read::SymbolId> {
    if requirements.requires(FactRequirement::AllOccurrences)
        || (requirements.requires(FactRequirement::CompleteDomain)
            && requirements
                .facets
                .contains(&rift_protocol::read::RelationshipFacet::References))
    {
        canonical_query_symbols(facts)
    } else {
        requirements.occurrences.clone()
    }
}

fn physical_query_targets(
    query: &rift_server::EngineQuery<'_>,
    facts: &QueryFacts,
) -> Result<Vec<rift_search::query::QueryProviderTarget>, RiftError> {
    let mut physical = Vec::new();
    for (provider, snapshot) in facts.providers.iter().enumerate() {
        for record in &snapshot.records {
            let symbol = facts
                .symbols
                .get(record.symbol_index)
                .ok_or_else(|| acquisition_failure("the captured provider symbol is absent"))?;
            if symbol.symbol.id.is_some() {
                continue;
            }
            let target = record.target().ok_or_else(|| {
                acquisition_failure("the captured provider record has no contribution")
            })?;
            physical.push(
                query
                    .provider_target(&rift_search::query::QueryProviderEndpoint {
                        provider: Some(provider),
                        target,
                    })
                    .ok_or_else(|| {
                        acquisition_failure("the query frontier lost its original provider context")
                    })?,
            );
        }
    }
    Ok(physical)
}

fn physical_requirements(
    requirements: &FactRequirements,
) -> Vec<(
    TraversalDirection,
    Vec<rift_protocol::read::RelationshipFacet>,
)> {
    let mut physical_requirements = Vec::new();
    if requirements.requires(FactRequirement::AllOccurrences)
        || (requirements.requires(FactRequirement::CompleteDomain)
            && requirements
                .facets
                .contains(&rift_protocol::read::RelationshipFacet::References))
    {
        physical_requirements.push((
            TraversalDirection::Incoming,
            vec![rift_protocol::read::RelationshipFacet::References],
        ));
    }
    if requirements.requires(FactRequirement::CompleteDomain) {
        let outgoing = requirements
            .facets
            .iter()
            .copied()
            .filter(|facet| *facet != rift_protocol::read::RelationshipFacet::References)
            .collect::<Vec<_>>();
        if !outgoing.is_empty() || requirements.facets.is_empty() {
            physical_requirements.push((TraversalDirection::Outgoing, outgoing));
        }
    }
    physical_requirements
}

fn query_adjacency(
    requirements: &FactRequirements,
    facts: &QueryFacts,
) -> Vec<rift_search::sparql::AdjacencyRequirement> {
    let mut adjacency = requirements.adjacency.clone();
    if requirements.requires(FactRequirement::CompleteDomain) {
        let incoming = requirements
            .facets
            .iter()
            .copied()
            .filter(|facet| *facet == rift_protocol::read::RelationshipFacet::References)
            .collect::<Vec<_>>();
        let outgoing = requirements
            .facets
            .iter()
            .copied()
            .filter(|facet| *facet != rift_protocol::read::RelationshipFacet::References)
            .collect::<Vec<_>>();
        for seed in canonical_query_symbols(facts) {
            if !incoming.is_empty() {
                adjacency.push(rift_search::sparql::AdjacencyRequirement {
                    seed: seed.clone(),
                    direction: TraversalDirection::Incoming,
                    facets: incoming.clone(),
                    depth: Some(1),
                });
            }
            if !outgoing.is_empty() || requirements.facets.is_empty() {
                adjacency.push(rift_search::sparql::AdjacencyRequirement {
                    seed,
                    direction: TraversalDirection::Outgoing,
                    facets: outgoing.clone(),
                    depth: None,
                });
            }
        }
    }
    adjacency
}

impl RiftMcp {
    async fn acquire_query(
        &self,
        input: ScopedQueryInput<'_>,
        requirements: &FactRequirements,
        facts: &QueryFacts,
        upstream: &[super::candidates::QueryGlobalSelection],
        selection: rift_search::query::QueryProviderSelection,
    ) -> Result<
        (
            rift_server::EngineReferences,
            Option<super::candidates::QueryGlobalSelection>,
            Vec<ReadWarning>,
        ),
        RiftError,
    > {
        let mut query = rift_server::EngineQuery::new(
            &input.resolved.published.reads,
            input.engines,
            input.accepted,
            input.execution.deadline.at(),
            input.roots,
        )
        .with_query_budget(input.execution.budget.clone())
        .with_query_facts(facts)?
        .with_provider_selection(selection)
        .with_provider_requests(input.execution.requests.clone());
        let requests_max = input.accepted.search.query.requests;
        let global_configuration = input
            .resolved
            .published
            .configuration
            .global_configuration();
        let seeds_max = usize::try_from(global_configuration.max_seeds)
            .ok()
            .filter(|count| *count > 0)
            .ok_or_else(|| {
                acquisition_failure("relationship subject bound exceeds the platform bound")
            })?;
        let mut remote = global::GlobalQuery::from_selected(
            upstream,
            &global_configuration,
            requirements.requires(FactRequirement::Source),
        )?;
        let occurrences = occurrence_subjects(requirements, facts);
        self.acquire_occurrence_subjects(&mut query, &mut remote, &input, &occurrences, seeds_max)
            .await?;
        self.acquire_physical_subjects(
            &mut query,
            &mut remote,
            &input,
            requirements,
            facts,
            seeds_max,
        )
        .await?;
        for requirement in query_adjacency(requirements, facts) {
            self.acquire_query_requirement(
                &mut query,
                &mut remote,
                &input,
                &requirement,
                (seeds_max, requests_max),
            )
            .await?;
        }
        self.validate_query_sources(&query, &input).await?;
        let selected = remote.selection(
            &input
                .resolved
                .published
                .configuration
                .global_configuration(),
        )?;
        Ok((query.into_references(), selected, remote.warnings))
    }

    async fn acquire_occurrence_subjects(
        &self,
        query: &mut rift_server::EngineQuery<'_>,
        remote: &mut global::GlobalQuery,
        input: &ScopedQueryInput<'_>,
        occurrences: &[rift_protocol::read::SymbolId],
        seeds_max: usize,
    ) -> Result<(), RiftError> {
        for targets in occurrences.chunks(seeds_max) {
            self.acquire_query_subjects(
                query,
                remote,
                input,
                targets,
                TraversalDirection::Incoming,
                &[rift_protocol::read::RelationshipFacet::References],
            )
            .await?;
            self.query_dependency_sources(
                query,
                remote,
                input.resolved,
                input.roots,
                input.accepted,
                input.execution.deadline,
            )
            .await?;
        }
        Ok(())
    }

    async fn acquire_physical_subjects(
        &self,
        query: &mut rift_server::EngineQuery<'_>,
        remote: &mut global::GlobalQuery,
        input: &ScopedQueryInput<'_>,
        requirements: &FactRequirements,
        facts: &QueryFacts,
        seeds_max: usize,
    ) -> Result<(), RiftError> {
        let physical = physical_query_targets(query, facts)?;
        let physical_requirements = physical_requirements(requirements);
        for (direction, facets) in physical_requirements {
            for targets in physical.chunks(seeds_max) {
                self.acquire_query_targets(query, remote, input, targets, direction, &facets)
                    .await?;
                self.query_dependency_sources(
                    query,
                    remote,
                    input.resolved,
                    input.roots,
                    input.accepted,
                    input.execution.deadline,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn acquire_query_requirement(
        &self,
        query: &mut rift_server::EngineQuery<'_>,
        remote: &mut global::GlobalQuery,
        input: &ScopedQueryInput<'_>,
        requirement: &rift_search::sparql::AdjacencyRequirement,
        bounds: (usize, u64),
    ) -> Result<(), RiftError> {
        let (seeds_max, requests_max) = bounds;
        let seed = rift_search::query::QueryProviderTarget::canonical(&requirement.seed)?;
        let seed = query
            .provider_endpoint(&seed)
            .ok_or_else(|| acquisition_failure("the query seed has no canonical identity"))?;
        let mut discovered = BTreeSet::from([seed.clone()]);
        let mut pending = VecDeque::from([(seed, 0_u32)]);
        let mut visited = BTreeSet::new();
        while let Some((subject, depth)) = pending.pop_front() {
            if !visited.insert(subject.clone())
                || requirement.depth.is_some_and(|bound| depth >= bound)
            {
                continue;
            }
            let mut subjects = vec![subject];
            while subjects.len() < seeds_max
                && pending
                    .front()
                    .is_some_and(|(_, next_depth)| *next_depth == depth)
            {
                let Some((next, _)) = pending.pop_front() else {
                    break;
                };
                if visited.insert(next.clone()) {
                    subjects.push(next);
                }
            }
            let targets = subjects
                .iter()
                .map(|subject| {
                    query.provider_target(subject).ok_or_else(|| {
                        acquisition_failure("the query frontier lost its original provider context")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.acquire_query_targets(
                query,
                remote,
                input,
                &targets,
                requirement.direction,
                &requirement.facets,
            )
            .await?;
            self.query_dependency_sources(
                query,
                remote,
                input.resolved,
                input.roots,
                input.accepted,
                input.execution.deadline,
            )
            .await?;
            self.name_pending_query_callees(query, input).await;
            for subject in subjects {
                for next in query.provider_neighbors(
                    &subject,
                    requirement.direction,
                    &requirement.facets,
                )? {
                    if discovered.insert(next.clone()) {
                        if u64::try_from(discovered.len())
                            .map_or(true, |count| count > requests_max)
                        {
                            return Err(acquisition_failure(
                                "query frontier exceeds search.query.requests; narrow the query or increase the accepted bound",
                            ));
                        }
                        pending.push_back((
                            next,
                            depth.checked_add(1).ok_or_else(|| {
                                acquisition_failure("query depth exceeds the platform bound")
                            })?,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    async fn name_pending_query_callees(
        &self,
        query: &mut rift_server::EngineQuery<'_>,
        input: &ScopedQueryInput<'_>,
    ) {
        if !query.references().package_callees().is_empty() {
            let (named, _) = self
                .package_callee_names(
                    input.resolved,
                    query.references().package_callees(),
                    input.execution.deadline,
                )
                .await;
            match named {
                Some(named) => {
                    query.name_package_callees(|callee| named.declaration(callee));
                }
                None => query.references_mut().drop_package_callees(),
            }
        }
    }

    async fn validate_query_sources(
        &self,
        query: &rift_server::EngineQuery<'_>,
        input: &ScopedQueryInput<'_>,
    ) -> Result<(), RiftError> {
        let targets = query.validation_targets();
        let roots = input.roots.clone();
        let trees = query.source_trees();
        let accepted = input.accepted.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        tokio::time::timeout_at(
            input.execution.deadline.at(),
            self.blocking.run_with_cancellation(
                "query dependency validation",
                cancellation,
                move |cancelled| {
                    let trees = trees
                        .iter()
                        .map(std::path::PathBuf::as_path)
                        .collect::<Vec<_>>();
                    for target in targets {
                        target.validate_with_cancellation(&roots, &trees, &accepted, &|| {
                            cancelled.is_cancelled()
                        })?;
                    }
                    Ok(())
                },
            ),
        )
        .await
        .map_err(|_| {
            acquisition_failure("dependency validation exceeded the request deadline")
        })??;
        drop(guard);
        Ok(())
    }

    async fn acquire_query_targets(
        &self,
        query: &mut rift_server::EngineQuery<'_>,
        remote: &mut global::GlobalQuery,
        input: &ScopedQueryInput<'_>,
        subjects: &[rift_search::query::QueryProviderTarget],
        direction: TraversalDirection,
        facets: &[rift_protocol::read::RelationshipFacet],
    ) -> Result<(), RiftError> {
        let canonical = subjects
            .iter()
            .filter_map(rift_search::query::QueryProviderTarget::canonical_symbol)
            .collect::<Vec<_>>();
        if !canonical.is_empty() {
            self.acquire_query_subjects(query, remote, input, &canonical, direction, facets)
                .await?;
        }
        let physical = subjects
            .iter()
            .filter(|subject| subject.context.is_some())
            .cloned()
            .collect::<Vec<_>>();
        if !physical.is_empty() {
            query.resolve_targets(&physical, direction, facets).await?;
        }
        Ok(())
    }

    async fn acquire_query_subjects(
        &self,
        query: &mut rift_server::EngineQuery<'_>,
        remote: &mut global::GlobalQuery,
        input: &ScopedQueryInput<'_>,
        subjects: &[rift_protocol::read::SymbolId],
        direction: TraversalDirection,
        facets: &[rift_protocol::read::RelationshipFacet],
    ) -> Result<(), RiftError> {
        let mut local = Vec::new();
        let mut released = Vec::new();
        for subject in subjects {
            if query.relationships_complete(subject, direction, facets) {
                continue;
            }
            if global::released(subject)? {
                released.push(subject.clone());
            } else {
                local.push(subject.clone());
            }
        }
        if !local.is_empty() {
            query.resolve_facets(&local, direction, facets).await?;
        }
        if !released.is_empty() {
            remote
                .prepare(self, input.resolved, query, input.execution.deadline)
                .await?;
            let supported = remote.supported_facets(direction, facets);
            let unsupported = facets
                .iter()
                .copied()
                .filter(|facet| !supported.contains(facet))
                .collect::<Vec<_>>();
            if !supported.is_empty() {
                remote
                    .acquire(self, input, query, &released, direction, &supported)
                    .await?;
            }
            if supported.is_empty() || !unsupported.is_empty() {
                query
                    .resolve_facets(
                        &released,
                        direction,
                        if facets.is_empty() {
                            facets
                        } else {
                            &unsupported
                        },
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn query_dependency_sources(
        &self,
        query: &mut rift_server::EngineQuery<'_>,
        remote: &mut global::GlobalQuery,
        resolved: &super::ResolvedWorkspace,
        roots: &rift_server::CalleeRoots,
        configuration: &rift_protocol::configuration::WorkspaceConfiguration,
        deadline: super::RequestDeadline,
    ) -> Result<(), RiftError> {
        let requests = query.source_requests();
        if requests.is_empty() {
            return Ok(());
        }
        remote.prepare(self, resolved, query, deadline).await?;
        for request in requests {
            let canonical = query.package_identity(&request).is_some();
            let roots = roots.clone();
            let trees = query.source_trees();
            let accepted = configuration.clone();
            let preparation_budget = query.query_budget();
            let preparation_selection = query
                .provider_selection()
                .ok_or_else(|| acquisition_failure("captured provider selection is unavailable"))?;
            let cancellation = tokio_util::sync::CancellationToken::new();
            let guard = cancellation.clone().drop_guard();
            let target = tokio::time::timeout_at(
                deadline.at(),
                self.blocking.run_with_cancellation(
                    "query dependency source",
                    cancellation,
                    move |cancelled| {
                        let cancelled = || cancelled.is_cancelled();
                        let mut budget = preparation_budget
                            .as_ref()
                            .map(|budget| budget.with_cancellation(&cancelled));
                        let trees = trees
                            .iter()
                            .map(std::path::PathBuf::as_path)
                            .collect::<Vec<_>>();
                        if canonical {
                            rift_server::QueryPackageTarget::prepare_with_selection(
                                request,
                                &roots,
                                &trees,
                                &accepted,
                                &cancelled,
                                budget.as_mut(),
                                Some(preparation_selection.as_ref()),
                            )
                        } else {
                            rift_server::QueryPackageTarget::prepare_selected_with_selection(
                                request,
                                &roots,
                                &trees,
                                &accepted,
                                &cancelled,
                                budget.as_mut(),
                                Some(preparation_selection.as_ref()),
                            )
                        }
                    },
                ),
            )
            .await
            .map_err(|_| {
                acquisition_failure("dependency source acquisition exceeded the request deadline")
            })??;
            drop(guard);
            query.admit_package_target(target)?;
        }
        Ok(())
    }
}

pub(super) fn acquisition_failure(detail: &'static str) -> RiftError {
    errors::server::read_unavailable()
        .operation("query acquisition")
        .detail(detail)
        .error()
}

fn query_limits(configuration: &QueryConfiguration) -> Result<QueryLimits, ToolFailure> {
    QueryLimits::try_from(configuration).map_err(failure)
}

fn failure(error: RiftError) -> ToolFailure {
    error.mcp().tool_failure(wire::ErrorPhase::Read)
}
