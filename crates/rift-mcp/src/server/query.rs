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

impl RiftMcp {
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

    pub(super) async fn graph_query(
        &self,
        params: QueryParams,
    ) -> Result<Json<QueryReadResult>, ToolFailure> {
        let deadline = self.request_deadline().await;
        let configuration = self
            .published
            .read()
            .await
            .current
            .configuration
            .search_configuration()
            .query;
        let limits = query_limits(&configuration)?;
        let budget = QueryBudget::for_request(limits, deadline.at().into_std(), &|| false)
            .map_err(failure)?;
        let program = self.query_program.clone().ok_or_else(|| {
            failure(
                errors::server::read_unsupported()
                    .capability("query execution requires a compatible Rift executable")
                    .error(),
            )
        })?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let prepared = tokio::time::timeout_at(
            deadline.at(),
            self.blocking
                .run_with_cancellation("query parse", cancellation, move |cancelled| {
                    rift_server::prepare_query(
                        program.as_path(),
                        params.query,
                        limits,
                        deadline.at().into_std(),
                        &|| cancelled.is_cancelled(),
                    )
                }),
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
        if let Some(selection) = prepared.page_selection() {
            if let Some(result) = self.retained_query_page(selection).await? {
                return Ok(Json(result));
            }
        }
        let selected = match self
            .acquire_query_data(
                prepared.requirements(),
                prepared.retrievals(),
                None,
                &configuration,
                limits,
                &budget,
                deadline,
                rift_server::ProviderRequests::new(configuration.requests),
            )
            .await?
        {
            Ok(selected) => selected,
            Err(reason) => return Ok(Json(QueryReadResult::Unavailable(reason))),
        };
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
    }

    async fn acquire_query_data(
        &self,
        requirements: &FactRequirements,
        retrievals: &[rift_protocol::read::SearchParams],
        selected_view: Option<(
            rift_server::LocalSymbolViewInput,
            rift_protocol::symbol_read::CapturedView,
        )>,
        configuration: &QueryConfiguration,
        limits: QueryLimits,
        budget: &QueryBudget,
        deadline: super::RequestDeadline,
        requests: rift_server::ProviderRequests,
    ) -> Result<Result<QueryAcquisition, SearchUnavailable>, ToolFailure> {
        let mut selected = match self
            .query_services(
                requirements,
                retrievals,
                configuration,
                budget,
                deadline,
                &requests,
            )
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
        let SelectedQueryServices {
            services,
            material: retained_material,
            warnings: retained_warnings,
            expiry,
            mut upstream,
        } = selected;
        let semantic = requires_engine(requirements);
        if !requirements.requires(FactRequirement::Domain) {
            let material = match retained_material {
                Some(material) => material,
                None => {
                    let published = Arc::clone(&self.published.read().await.current);
                    let reads = Arc::clone(&published.reads);
                    let ready = published.preparation.is_none();
                    let accepted = published
                        .configuration
                        .accepted_for_tool(wire::ErrorPhase::Read)?;
                    self.blocking
                        .run("query selection", move || {
                            let material = reads.local_search_view_input(ready)?;
                            let selection = material.provider_selection(&accepted)?;
                            material.with_provider_selection(selection)
                        })
                        .await
                        .map_err(failure)?
                }
            };
            let material = limit_material(material, expiry.as_ref())?;
            return Ok(Ok(QueryAcquisition {
                facts: QueryFacts::default(),
                material,
                services,
                warnings: retained_warnings,
                upstream,
            }));
        }
        let retained_material = if !semantic {
            if let Some(material) = retained_material {
                let material = limit_material(material, expiry.as_ref())?;
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
                            let selection =
                                material.captured_provider_selection().ok_or_else(|| {
                                    acquisition_failure(
                                        "captured provider selection is unavailable",
                                    )
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
                return Ok(Ok(QueryAcquisition {
                    facts,
                    material,
                    services,
                    warnings: retained_warnings,
                    upstream,
                }));
            }
            None
        } else {
            retained_material
        };
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
        let accepted = resolved
            .published
            .configuration
            .accepted_for_tool(wire::ErrorPhase::Read)?;
        let reads = Arc::clone(&resolved.published.reads);
        let projection_requirements = requirements.clone();
        let projection_configuration = accepted.clone();
        let ((mut facts, material, provider_selection), mut warnings) = {
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
                        Ok((
                            material.query_facts_with_selection(
                                &projection_requirements,
                                limits,
                                &mut budget,
                                &selection,
                            )?,
                            material,
                            selection,
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
            (
                projection,
                resolved.stale.clone().into_iter().collect::<Vec<_>>(),
            )
        };
        let material = match retained_material {
            Some(selected) => self.query_work(budget, deadline, move |budget| {
                for facts in [material.package_facts(), selected.package_facts()].into_iter().flatten() {
                    budget.charge(facts.as_ref())?;
                }
                material.combine(selected).map_err(|_| acquisition_failure("selected query facts belong to different publications; select one retained context"))
            }).await?,
            None => material,
        };
        let material = limit_material(material, expiry.as_ref())?;
        warnings.extend(retained_warnings);
        let (references, global_selection, global_warnings) = if semantic {
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
            self.acquire_query(
                &resolved,
                &engines,
                &roots,
                requirements,
                &facts,
                &accepted,
                &upstream,
                budget,
                deadline,
                provider_selection,
                requests,
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
            || (semantic && !self.engine_tree_matches(&resolved.published).await?)
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
            .query_work(budget, deadline, move |budget| {
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
                budget.charge(package_facts)?;
                if let Some(existing) = material.package_facts() {
                    budget.charge(existing.as_ref())?;
                }
                let material = material.with_package_facts(package_facts.clone())?;
                Ok((facts, material))
            })
            .await?;
        Ok(Ok(QueryAcquisition {
            facts,
            material,
            services,
            warnings,
            upstream,
        }))
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
        configuration: &QueryConfiguration,
        budget: &QueryBudget,
        deadline: super::RequestDeadline,
        requests: &rift_server::ProviderRequests,
    ) -> Result<Result<SelectedQueryServices, SearchUnavailable>, ToolFailure> {
        let mut selected = SelectedQueryServices::default();
        let global_configuration = self
            .published
            .read()
            .await
            .current
            .configuration
            .global_configuration();
        for request in retrievals {
            let result = tokio::time::timeout_at(
                deadline.at(),
                self.complete_retrieval(request, configuration, requests, budget, deadline),
            )
            .await
            .map_err(|_| {
                failure(acquisition_failure(
                    "retrieval acquisition exceeded the request deadline",
                ))
            })??;
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
                            selection::hit_selection(hit, &global_configuration).map_err(failure)?
                            && let Err(reason) =
                                selection::include(&mut selected.upstream, upstream)
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
                    });
                }
                rift_protocol::search_read::SearchReadResult::Unavailable(reason) => {
                    return Ok(Err(reason));
                }
            }
        }
        for service in &requirements.services {
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
    warnings: Vec<ReadWarning>,
    upstream: Vec<super::candidates::QueryGlobalSelection>,
}

#[derive(Default)]
struct SelectedQueryServices {
    services: QueryServices,
    material: Option<rift_server::LocalSymbolViewInput>,
    warnings: Vec<rift_protocol::read::ReadWarning>,
    expiry: Option<rift_protocol::symbol_read::CapturedView>,
    upstream: Vec<super::candidates::QueryGlobalSelection>,
}

impl SelectedQueryServices {
    fn include_material(
        &mut self,
        material: rift_server::LocalSymbolViewInput,
    ) -> Result<(), SymbolUnavailableReason> {
        self.material = Some(match self.material.take() {
            Some(selected) => selected.combine(material)?,
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

impl RiftMcp {
    async fn acquire_query(
        &self,
        resolved: &super::ResolvedWorkspace,
        engines: &rift_server::EnginePool,
        roots: &rift_server::CalleeRoots,
        requirements: &FactRequirements,
        facts: &QueryFacts,
        configuration: &rift_protocol::configuration::WorkspaceConfiguration,
        upstream: &[super::candidates::QueryGlobalSelection],
        budget: &QueryBudget,
        deadline: super::RequestDeadline,
        selection: rift_search::query::QueryProviderSelection,
        requests: rift_server::ProviderRequests,
    ) -> Result<
        (
            rift_server::EngineReferences,
            Option<super::candidates::QueryGlobalSelection>,
            Vec<ReadWarning>,
        ),
        RiftError,
    > {
        let mut query = rift_server::EngineQuery::new(
            &resolved.published.reads,
            engines,
            configuration,
            deadline.at(),
            roots,
        )
        .with_query_budget(budget.clone())
        .with_provider_selection(selection)
        .with_provider_requests(requests);
        let requests_max = configuration.search.query.requests;
        let mut remote = global::GlobalQuery::from_selected(
            upstream,
            &resolved.published.configuration.global_configuration(),
            requirements.requires(FactRequirement::Source),
        )?;
        let symbols = || {
            facts
                .symbols
                .iter()
                .filter_map(|item| item.symbol.id.clone())
                .collect::<Vec<_>>()
        };
        let occurrences = if requirements.requires(FactRequirement::AllOccurrences)
            || (requirements.requires(FactRequirement::CompleteDomain)
                && requirements
                    .facets
                    .contains(&rift_protocol::read::RelationshipFacet::References))
        {
            symbols()
        } else {
            requirements.occurrences.clone()
        };
        for targets in occurrences.chunks(rift_protocol::relationship_read::RELATIONSHIP_SEEDS_MAX)
        {
            self.acquire_query_subjects(
                &mut query,
                &mut remote,
                resolved,
                targets,
                TraversalDirection::Incoming,
                &[rift_protocol::read::RelationshipFacet::References],
                deadline,
            )
            .await?;
            self.query_dependency_sources(
                &mut query,
                &mut remote,
                resolved,
                roots,
                configuration,
                deadline,
            )
            .await?;
        }
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
            for seed in symbols() {
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
        for requirement in adjacency {
            let seed = rift_search::query::QueryProviderTarget::canonical(&requirement.seed)?;
            let seed = query
                .references()
                .query_package_facts()
                .provider_endpoint(&seed)
                .ok_or_else(|| acquisition_failure("the query seed has no canonical identity"))?;
            let mut discovered = BTreeSet::from([seed.clone()]);
            let mut pending = VecDeque::from([(seed, 0_u32)]);
            let mut seen = BTreeSet::new();
            while let Some((subject, depth)) = pending.pop_front() {
                if !seen.insert(subject.clone())
                    || requirement.depth.is_some_and(|bound| depth >= bound)
                {
                    continue;
                }
                let mut subjects = vec![subject];
                while subjects.len() < rift_protocol::relationship_read::RELATIONSHIP_SEEDS_MAX
                    && pending
                        .front()
                        .is_some_and(|(_, next_depth)| *next_depth == depth)
                {
                    let Some((next, _)) = pending.pop_front() else {
                        break;
                    };
                    if seen.insert(next.clone()) {
                        subjects.push(next);
                    }
                }
                let targets = subjects
                    .iter()
                    .map(|subject| {
                        query
                            .references()
                            .query_package_facts()
                            .provider_target(subject)
                            .ok_or_else(|| {
                                acquisition_failure(
                                    "the query frontier lost its original provider context",
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                self.acquire_query_targets(
                    &mut query,
                    &mut remote,
                    resolved,
                    &targets,
                    requirement.direction,
                    &requirement.facets,
                    deadline,
                )
                .await?;
                self.query_dependency_sources(
                    &mut query,
                    &mut remote,
                    resolved,
                    roots,
                    configuration,
                    deadline,
                )
                .await?;
                if !query.references().package_callees().is_empty() {
                    let (named, _) = self
                        .package_callee_names(
                            resolved,
                            query.references().package_callees(),
                            deadline,
                        )
                        .await;
                    match named {
                        Some(named) => {
                            query.name_package_callees(|callee| named.declaration(callee))
                        }
                        None => query.references_mut().drop_package_callees(),
                    }
                }
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
        }
        let targets = query.validation_targets();
        let roots = roots.clone();
        let trees = query.source_trees();
        let accepted = configuration.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        tokio::time::timeout_at(
            deadline.at(),
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
        let selected =
            remote.selection(&resolved.published.configuration.global_configuration())?;
        Ok((query.into_references(), selected, remote.warnings))
    }

    async fn acquire_query_targets(
        &self,
        query: &mut rift_server::EngineQuery<'_>,
        remote: &mut global::GlobalQuery,
        resolved: &super::ResolvedWorkspace,
        subjects: &[rift_search::query::QueryProviderTarget],
        direction: TraversalDirection,
        facets: &[rift_protocol::read::RelationshipFacet],
        deadline: super::RequestDeadline,
    ) -> Result<(), RiftError> {
        let canonical = subjects
            .iter()
            .filter_map(|subject| subject.canonical_symbol())
            .collect::<Vec<_>>();
        if !canonical.is_empty() {
            self.acquire_query_subjects(
                query, remote, resolved, &canonical, direction, facets, deadline,
            )
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
        resolved: &super::ResolvedWorkspace,
        subjects: &[rift_protocol::read::SymbolId],
        direction: TraversalDirection,
        facets: &[rift_protocol::read::RelationshipFacet],
        deadline: super::RequestDeadline,
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
            remote.prepare(self, resolved, query, deadline).await?;
            let supported = remote.supported_facets(direction, facets);
            let unsupported = facets
                .iter()
                .copied()
                .filter(|facet| !supported.contains(facet))
                .collect::<Vec<_>>();
            if !supported.is_empty() {
                remote
                    .acquire(
                        self, resolved, query, &released, direction, &supported, deadline,
                    )
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
    let quantity = |value| {
        usize::try_from(value).map_err(|_| {
            failure(acquisition_failure(
                "query configuration exceeds the platform bound",
            ))
        })
    };
    Ok(QueryLimits {
        query_bytes_max: quantity(configuration.statement_size.bytes())?,
        algebra_nodes_max: quantity(configuration.algebra_nodes)?,
        quads_max: quantity(configuration.statements)?,
        rdf_bytes_max: quantity(configuration.fact_size.bytes())?,
        result_items_max: quantity(configuration.results)?,
        result_bytes_max: quantity(configuration.result_size.bytes())?,
    })
}

fn failure(error: RiftError) -> ToolFailure {
    error.mcp().tool_failure(wire::ErrorPhase::Read)
}
