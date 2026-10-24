//! Query acquisition and execution under the existing publication and worker owners.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use rift_error::{RiftError, errors};
use rift_protocol::query::{QueryConfiguration, QueryParams, QueryReadResult};
use rift_protocol::read::{ReadWarning, TraversalDirection};
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::SymbolUnavailableReason;
use rift_search::sparql::{
    FactRequirements, LOCAL_GRAPH, PreparedQuery, QueryCancellation, QueryFacts, QueryLimits,
    QueryServiceData, QueryServices, RESULT_SERVICE_PREFIX, RetainedResult,
};

use super::{Json, RiftMcp, ToolFailure, wire};
use crate::failure::McpErrorExt as _;

mod global;
mod retrieval;
mod selection;

impl RiftMcp {
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
        let prepared = self
            .blocking
            .run("query parse", move || {
                PreparedQuery::parse(&params.query, limits)
            })
            .await
            .map_err(failure)?;
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
        let SelectedQueryServices {
            services,
            material: retained_material,
            warnings: retained_warnings,
            expiry,
            mut upstream,
        } = match self
            .query_services(&prepared, &configuration, deadline)
            .await?
        {
            Ok(selected) => selected,
            Err(reason) => return Ok(Json(QueryReadResult::Unavailable(reason))),
        };
        let semantic = requires_engine(prepared.requirements());
        if !prepared.requirements().domain {
            let material = match retained_material {
                Some(material) => material,
                None => {
                    let published = Arc::clone(&self.published.read().await.current);
                    let reads = Arc::clone(&published.reads);
                    let ready = published.preparation.is_none();
                    self.blocking
                        .run("query selection", move || {
                            reads.local_search_view_input(ready)
                        })
                        .await
                        .map_err(failure)?
                }
            };
            let material = limit_material(material, expiry.as_ref())?;
            let answer = self
                .evaluate_query(prepared, QueryFacts::default(), limits, services, deadline)
                .await?;
            return self
                .retain_query(
                    material,
                    answer,
                    &configuration,
                    retained_warnings,
                    upstream,
                )
                .await
                .map(Json);
        }
        let retained_material = if !semantic {
            if let Some(material) = retained_material {
                let material = limit_material(material, expiry.as_ref())?;
                let requirements = prepared.requirements().clone();
                let (facts, material) = self
                    .blocking
                    .run("retained query facts", move || {
                        Ok((material.query_facts(&requirements, limits)?, material))
                    })
                    .await
                    .map_err(failure)?;
                let answer = self
                    .evaluate_query(prepared, facts, limits, services, deadline)
                    .await?;
                return self
                    .retain_query(
                        material,
                        answer,
                        &configuration,
                        retained_warnings,
                        upstream,
                    )
                    .await
                    .map(Json);
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
            != configuration
        {
            return Err(failure(acquisition_failure(
                "query configuration changed during parsing; resend the same request",
            )));
        }
        if resolved.published.preparation.is_some() {
            return Ok(Json(QueryReadResult::Unavailable(
                SearchUnavailable::Unavailable {
                    reason: SymbolUnavailableReason::IndexPreparing,
                    view: None,
                    warnings: resolved.stale.into_iter().collect(),
                },
            )));
        }
        let reads = Arc::clone(&resolved.published.reads);
        let requirements = prepared.requirements().clone();
        let ((mut facts, material), mut warnings) = {
            let projection = self
                .blocking
                .run("query facts", move || {
                    let material = reads.local_search_view_input(true)?;
                    Ok((material.query_facts(&requirements, limits)?, material))
                })
                .await
                .map_err(failure)?;
            (
                projection,
                resolved.stale.clone().into_iter().collect::<Vec<_>>(),
            )
        };
        let material = match retained_material {
            Some(selected) => material.combine(selected).map_err(|_| failure(acquisition_failure("selected query facts belong to different publications; select one retained context")))?,
            None => material,
        };
        let material = limit_material(material, expiry.as_ref())?;
        warnings.extend(retained_warnings);
        let (references, global_selection, global_warnings) = if semantic {
            let engines = self.engine_pool_for(&resolved.published).await;
            let roots_reads = Arc::clone(&resolved.published.reads);
            let roots_engines = Arc::clone(&engines);
            let accepted = resolved
                .published
                .configuration
                .accepted_for_tool(wire::ErrorPhase::Read)?;
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
                prepared.requirements(),
                &facts,
                &accepted,
                &upstream,
                deadline,
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
            return Ok(Json(QueryReadResult::Unavailable(
                SearchUnavailable::Unavailable {
                    reason,
                    view: None,
                    warnings,
                },
            )));
        }
        warnings.extend(references.warnings());
        if !references.is_complete()
            || (semantic && !self.engine_tree_matches(&resolved.published).await?)
        {
            return Ok(Json(QueryReadResult::Unavailable(
                SearchUnavailable::Unavailable {
                    reason: SymbolUnavailableReason::InsufficientCoverage,
                    view: None,
                    warnings,
                },
            )));
        }
        facts
            .relationships
            .extend(references.query_local_relationships().cloned());
        facts
            .occurrences
            .extend_from_slice(references.query_occurrences());
        let package_facts = references.query_package_facts();
        facts.symbols.extend_from_slice(&package_facts.symbols);
        facts.sources.extend_from_slice(&package_facts.sources);
        facts
            .relationship_records
            .extend_from_slice(&package_facts.relationship_records);
        let material = limit_material(
            material,
            global_selection.as_ref().map(|selected| &selected.view),
        )?
        .with_package_facts(package_facts.clone())
        .map_err(failure)?;
        let answer = self
            .evaluate_query(prepared, facts, limits, services, deadline)
            .await?;
        self.retain_query(material, answer, &configuration, warnings, upstream)
            .await
            .map(Json)
    }

    async fn evaluate_query(
        &self,
        prepared: PreparedQuery,
        facts: QueryFacts,
        limits: QueryLimits,
        services: QueryServices,
        deadline: super::RequestDeadline,
    ) -> Result<rift_protocol::query::QueryAnswer, ToolFailure> {
        let cancellation = QueryCancellation::default();
        let guard = QueryCancellationGuard(cancellation.clone());
        let answer = tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run("query evaluate", move || {
                prepared.evaluate_with_services(facts, limits, cancellation, services)
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

    async fn query_services(
        &self,
        prepared: &PreparedQuery,
        configuration: &QueryConfiguration,
        deadline: super::RequestDeadline,
    ) -> Result<Result<SelectedQueryServices, SearchUnavailable>, ToolFailure> {
        let mut selected = SelectedQueryServices::default();
        let global_configuration = self
            .published
            .read()
            .await
            .current
            .configuration
            .global_configuration();
        let mut requests = 0_u64;
        for request in prepared.retrievals() {
            let result = tokio::time::timeout_at(
                deadline.at(),
                self.complete_retrieval(request, configuration, &mut requests),
            )
            .await
            .map_err(|_| {
                failure(acquisition_failure(
                    "retrieval acquisition exceeded the request deadline",
                ))
            })??;
            match result {
                rift_protocol::search_read::SearchReadResult::Results(result) => {
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
                                if let Err(reason) = selected.include_material(material) {
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
        for service in &prepared.requirements().services {
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
            let retained = match self.retained_query(&view, page).await? {
                Ok(retained) => retained,
                Err(reason) => {
                    return Ok(Err(SearchUnavailable::Unavailable {
                        reason,
                        view: None,
                        warnings: Vec::new(),
                    }));
                }
            };
            if let Err(reason) = selected.include_material(retained.material) {
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
    requirements.complete_domain
        || requirements.all_occurrences
        || !requirements.occurrences.is_empty()
        || !requirements.adjacency.is_empty()
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
        deadline: super::RequestDeadline,
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
        );
        let requests_max = configuration.search.query.requests;
        let mut remote = global::GlobalQuery::from_selected(
            upstream,
            &resolved.published.configuration.global_configuration(),
            requirements.source,
        )?;
        let symbols = || {
            facts
                .symbols
                .iter()
                .filter_map(|item| item.symbol.id.clone())
                .collect::<Vec<_>>()
        };
        let occurrences = if requirements.all_occurrences
            || (requirements.complete_domain
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
        if requirements.complete_domain {
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
            let mut discovered = BTreeSet::from([requirement.seed.clone()]);
            let mut pending = VecDeque::from([(requirement.seed, 0_u32)]);
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
                self.acquire_query_subjects(
                    &mut query,
                    &mut remote,
                    resolved,
                    &subjects,
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
                    for edge in query.references().query_relationships() {
                        let follows = match requirement.direction {
                            TraversalDirection::Outgoing => edge.from == subject,
                            TraversalDirection::Incoming => edge.to == subject,
                        };
                        if !follows {
                            continue;
                        }
                        if requirement.facets.is_empty()
                            || edge
                                .facets
                                .iter()
                                .any(|facet| requirement.facets.contains(facet))
                        {
                            let next = match requirement.direction {
                                TraversalDirection::Outgoing => edge.to,
                                TraversalDirection::Incoming => edge.from,
                            };
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
                                        acquisition_failure(
                                            "query depth exceeds the platform bound",
                                        )
                                    })?,
                                ));
                            }
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
            let roots = roots.clone();
            let trees = query.source_trees();
            let accepted = configuration.clone();
            let cancellation = tokio_util::sync::CancellationToken::new();
            let guard = cancellation.clone().drop_guard();
            let target = tokio::time::timeout_at(
                deadline.at(),
                self.blocking.run_with_cancellation(
                    "query dependency source",
                    cancellation,
                    move |cancelled| {
                        let trees = trees
                            .iter()
                            .map(std::path::PathBuf::as_path)
                            .collect::<Vec<_>>();
                        rift_server::QueryPackageTarget::prepare_with_cancellation(
                            request,
                            &roots,
                            &trees,
                            &accepted,
                            &|| cancelled.is_cancelled(),
                        )
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

struct QueryCancellationGuard(QueryCancellation);
impl Drop for QueryCancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
