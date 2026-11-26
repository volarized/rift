//! Graph query execution and retention under the captured workspace owners.

use std::sync::Arc;
use std::time::SystemTime;

use sha2::{Digest as _, Sha256};

use rift_error::{RiftError, errors};
use rift_protocol::query::{
    QueryAnswer, QueryConfiguration, QueryParams, QueryReadResult, QueryResult,
};
use rift_protocol::read::{Pagination, ReadWarning, SearchResult};
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use rift_search::sparql::{
    AdjacencySelection, GraphDataset, GraphLimits, GraphQuery, QueryAnswerProjection,
    QueryCancellation, SemanticInventory,
};
use rift_server::InventoryCoverage;

use super::{RiftMcp, read_failure};
use crate::output::{Json, QueryDisplay, QueryOutput, ToolFailure};

mod edges;
mod exact;
mod installed;
#[cfg(test)]
mod inventory_tests;
pub(crate) mod retrieval;

use super::super::RequestDeadline;
use edges::AcquiredAdjacency;
pub(crate) use edges::EngineInventoryCache;

/// Settings and presentation retained with one query execution.
pub(crate) struct QueryPageOptions<'a> {
    pub(crate) include: rift_protocol::query::QueryInclude,
    pub(crate) configuration: &'a QueryConfiguration,
    pub(crate) page: u64,
    pub(crate) max_results: u64,
    pub(crate) warnings: Vec<ReadWarning>,
    pub(crate) display: QueryDisplay,
}

struct QueryGuard(QueryCancellation);

impl Drop for QueryGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl RiftMcp {
    pub(in crate::server) async fn graph_query(
        &self,
        params: QueryParams,
    ) -> Result<Json<QueryOutput>, ToolFailure> {
        rift_server::accept_page(params.page).map_err(read_failure)?;
        rift_server::accepted_max_results(
            params.max_results,
            rift_protocol::query::QUERY_MAX_RESULTS,
        )
        .map_err(read_failure)?;
        let paging = (params.page, params.max_results, params.include);
        let deadline = self.request_deadline().await;
        let limits = GraphLimits::default();
        let query = tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run("graph query parse", move || {
                GraphQuery::parse(&params.query, &limits)
            }),
        )
        .await
        .map_err(|_| read_failure(deadline_failure(deadline.budget())))?
        .map_err(read_failure)?;
        let resolved = self
            .published_workspace(super::super::wire::ErrorPhase::Read, deadline)
            .await?;
        resolved
            .published
            .reads
            .accept_revision(params.revision.as_ref())
            .map_err(read_failure)?;
        let revision = resolved
            .published
            .reads
            .served_revision()
            .map_err(read_failure)?;
        let default_scope = resolved
            .published
            .configuration
            .accepted_for_tool(super::super::wire::ErrorPhase::Read)?
            .search
            .default_scope;
        // Every read of the query's retrieval requests comes after this line, so each
        // one meets the scope the accepted workspace selects.
        let query = query.with_default_scope(&default_scope);
        let key = query_key(&query, params.language, &revision, resolved.published.epoch)
            .map_err(read_failure)?;
        let configuration = resolved
            .published
            .configuration
            .search_configuration()
            .query;
        if params.revision.as_ref() == Some(&revision)
            && let Some(output) = self
                .retained_query_page(key, paging, revision.clone(), configuration, deadline)
                .await?
        {
            return Ok(output);
        }
        let graph = match Box::pin(self.prepare_graph(resolved, &query, limits, deadline)).await? {
            Ok(graph) => graph,
            Err(unavailable) => return Ok(Json(QueryReadResult::Unavailable(unavailable).into())),
        };
        let views = Arc::clone(&self.local_views);
        let cancellation = QueryCancellation::new();
        let _guard = QueryGuard(cancellation.clone());
        tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run("graph query", move || {
                graph.evaluate(&query, deadline, &cancellation, &views, paging, key)
            }),
        )
        .await
        .map_err(|_| read_failure(deadline_failure(deadline.budget())))?
        .map(|mut output| {
            if let QueryReadResult::Results(answer) = &mut output.result {
                answer.revision = Some(revision);
            }
            Json(output)
        })
        .map_err(read_failure)
    }

    async fn retained_query_page(
        &self,
        key: [u8; 32],
        paging: (u64, u64, rift_protocol::query::QueryInclude),
        revision: rift_protocol::revision::Revision,
        configuration: QueryConfiguration,
        deadline: RequestDeadline,
    ) -> Result<Option<Json<QueryOutput>>, ToolFailure> {
        let views = Arc::clone(&self.local_views);
        tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run("retained query page", move || {
                let Some((view, bytes)) = views.get().and_then(|views| views.query(&key)) else {
                    return Ok(None);
                };
                let (answer, warnings, display): (QueryAnswer, Vec<ReadWarning>, QueryDisplay) =
                    serde_json::from_slice(&bytes).map_err(encoding_failure)?;
                let mut output = query_page(
                    &view,
                    &answer,
                    QueryPageOptions {
                        include: paging.2,
                        configuration: &configuration,
                        page: paging.0,
                        max_results: paging.1,
                        warnings,
                        display,
                    },
                )?;
                if let QueryReadResult::Results(answer) = &mut output.result {
                    answer.revision = Some(revision);
                }
                Ok(Some(Json(output)))
            }),
        )
        .await
        .map_err(|_| read_failure(deadline_failure(deadline.budget())))?
        .map_err(read_failure)
    }

    /// Acquires what `query` reads from `resolved`, the publication whose accepted
    /// configuration already selected the query's default scope.
    async fn prepare_graph(
        &self,
        resolved: super::super::ResolvedWorkspace,
        query: &GraphQuery,
        limits: GraphLimits,
        deadline: RequestDeadline,
    ) -> Result<Result<AcquiredGraph, SearchUnavailable>, ToolFailure> {
        if let Some(preparation) = &resolved.published.preparation {
            return Ok(Err(preparation.unavailable()));
        }
        let mut selected = query.adjacency_selection();
        let local_semantics = if selected.is_none() || query.names_local_graph() {
            query.semantic_inventory()
        } else {
            None
        };
        if local_semantics.is_some() {
            selected = None;
        }
        let mut acquired = match self
            .prepare_edges(
                &resolved,
                query,
                selected.as_ref(),
                local_semantics,
                limits,
                deadline,
            )
            .await?
        {
            Ok(acquired) => acquired,
            Err(reason) => return Ok(Err(reason)),
        };
        let mut exact = match tokio::time::timeout_at(
            deadline.at(),
            Box::pin(self.acquire_exact_graph(&resolved, query, limits)),
        )
        .await
        .map_err(|_| read_failure(deadline_failure(deadline.budget())))?
        {
            Ok(Ok(exact)) => exact,
            Ok(Err(reason)) => return Ok(Err(reason)),
            Err(error) if lacks_coverage(&error) => {
                return Ok(Err(unavailable(
                    SymbolUnavailableReason::InsufficientCoverage,
                )));
            }
            Err(error) => return Err(read_failure(error)),
        };
        let configuration = resolved
            .published
            .configuration
            .search_configuration()
            .query;
        if !configuration.local_symbol_inventory
            && selected.is_none()
            && local_semantics.is_none()
            && exact.is_none()
            && query.symbol_selection().is_some()
        {
            return Ok(Err(coverage_unavailable(vec![
                ReadWarning::LocalSymbolInventoryUnavailable,
            ])));
        }
        let retrieved = match tokio::time::timeout_at(
            deadline.at(),
            Box::pin(self.prepare_retrievals(&resolved, query, limits, deadline)),
        )
        .await
        .map_err(|_| read_failure(deadline_failure(deadline.budget())))??
        {
            Ok(retrieved) => retrieved,
            Err(reason) => return Ok(Err(reason)),
        };
        if !retrieved.is_empty() && !self.engine_tree_matches(&resolved.published).await? {
            return Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
        }
        let reads = Arc::clone(&resolved.published.reads);
        let mut warnings = acquired.take_warnings();
        if local_semantics.is_some() {
            warnings.extend(reads.edge_identity_warning());
        }
        warnings.extend(resolved.stale);
        if let Some(exact) = &mut exact {
            warnings.extend(exact.take_warnings());
        }
        for warning in retrieved.iter().flat_map(|entry| &entry.result.warnings) {
            if !warnings.contains(warning) {
                warnings.push(warning.clone());
            }
        }
        Ok(Ok(AcquiredGraph {
            limits,
            reads,
            selected,
            local_semantics,
            acquired,
            exact,
            retrieved,
            configuration,
            warnings,
        }))
    }

    async fn prepare_retrievals(
        &self,
        resolved: &super::super::ResolvedWorkspace,
        query: &GraphQuery,
        limits: GraphLimits,
        deadline: RequestDeadline,
    ) -> Result<Result<Vec<AcquiredRetrieval>, SearchUnavailable>, ToolFailure> {
        use edges::FlowAcquisition;

        let mut retrieved = Vec::new();
        let mut acquisition = FlowAcquisition::new(self, resolved, limits, deadline);
        for selection in query.retrieval_requests() {
            let result = match self.query_retrieval(selection, limits).await? {
                Ok(result) => result,
                Err(reason) => return Ok(Err(reason)),
            };
            let flows = if let Some(limit) = selection.flow_limit {
                let ranked = result.as_ref();
                let ranked_rows = ranked.results.len();
                let flow_acquisition = &mut acquisition;
                let acquired = rift_tracing::traced!(
                    component = "search",
                    operation = "search.flows.acquire",
                    ranked_rows = ranked_rows,
                    async move { Box::pin(flow_acquisition.acquire(ranked)).await }
                )
                .await;
                match acquired {
                    Ok(flows) => Some((limit, flows)),
                    Err(error) if lacks_coverage(&error) => {
                        return Ok(Err(unavailable(
                            SymbolUnavailableReason::InsufficientCoverage,
                        )));
                    }
                    Err(error) => return Err(read_failure(error)),
                }
            } else {
                None
            };
            retrieved.push(AcquiredRetrieval { result, flows });
        }
        acquisition.validate().await.map_err(read_failure)?;
        Ok(Ok(retrieved))
    }
    async fn prepare_edges(
        &self,
        resolved: &super::super::ResolvedWorkspace,
        query: &GraphQuery,
        selected: Option<&AdjacencySelection>,
        local_semantics: Option<SemanticInventory>,
        limits: GraphLimits,
        deadline: RequestDeadline,
    ) -> Result<Result<AcquiredAdjacency, SearchUnavailable>, ToolFailure> {
        let acquire = async {
            if let Some(selected) = selected {
                self.acquire_adjacency(resolved, selected, limits, deadline)
                    .await
            } else if let Some(required) = local_semantics {
                Ok(Ok(self
                    .acquire_semantic_inventory(resolved, required, deadline)
                    .await?))
            } else if query.symbol_selection().is_some()
                || query.history_selection().is_some()
                || query.syntax_element_selection().is_some()
                || !query.reads_base()
            {
                Ok(Ok(AcquiredAdjacency::default()))
            } else {
                Err(edges::coverage_failure())
            }
        };
        let result = tokio::time::timeout_at(deadline.at(), Box::pin(acquire))
            .await
            .map_err(|_| read_failure(deadline_failure(deadline.budget())))?;
        match result {
            Ok(acquired) => Ok(acquired),
            Err(error) if lacks_coverage(&error) => Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            ))),
            Err(error) => Err(read_failure(error)),
        }
    }
}

struct AcquiredGraph {
    limits: GraphLimits,
    reads: Arc<rift_server::ReadService>,
    selected: Option<AdjacencySelection>,
    local_semantics: Option<SemanticInventory>,
    acquired: AcquiredAdjacency,
    exact: Option<exact::ExactFacts>,
    retrieved: Vec<AcquiredRetrieval>,
    configuration: QueryConfiguration,
    warnings: Vec<ReadWarning>,
}

struct AcquiredRetrieval {
    result: Box<SearchResult>,
    flows: Option<(usize, edges::AcquiredFlows)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectedRetrievals {
    Complete,
    Partial,
    Unavailable,
}

fn project_retrievals(
    query: &GraphQuery,
    retrieved: &mut [AcquiredRetrieval],
    dataset: &mut rift_search::sparql::GraphDatasetBuilder,
    warnings: &mut Vec<ReadWarning>,
) -> Result<ProjectedRetrievals, RiftError> {
    let mut coverage = ProjectedRetrievals::Complete;
    for (selection, entry) in query.retrieval_requests().iter().zip(retrieved) {
        let Some((limit, flows)) = entry.flows.take() else {
            dataset.insert_retrieval(selection, &entry.result)?;
            continue;
        };
        let (flows, partial) = flows.finish(limit, warnings);
        if partial {
            if flows.paths.is_empty() || !query.admits_partial_rows() {
                return Ok(ProjectedRetrievals::Unavailable);
            }
            coverage = ProjectedRetrievals::Partial;
        }
        dataset.insert_flows(selection, &flows)?;
    }
    Ok(coverage)
}

fn project_local_endpoints(
    input: &rift_server::LocalSymbolViewInput,
    reads: &rift_server::ReadService,
    acquired: &edges::AcquiredAdjacency,
    selected: &rift_search::sparql::AdjacencySelection,
    dataset: &mut rift_search::sparql::GraphDatasetBuilder,
    cancellation: &QueryCancellation,
    deadline: std::time::Instant,
) -> Result<(), RiftError> {
    if !selected.symbol_details && selected.syntax_elements.is_none() {
        return Ok(());
    }
    for endpoint in acquired.local_endpoints() {
        let identity = rift_protocol::identity::SymbolIdentity::parse(endpoint.as_str())?;
        if identity.owner().is_local() {
            let symbols = rift_search::sparql::SymbolSelection::Identified(endpoint.as_str());
            if let Some(syntax_elements) = selected.syntax_elements {
                input.project_syntax_elements(
                    reads,
                    rift_search::sparql::SyntaxSelection {
                        symbols,
                        source: syntax_elements.source,
                    },
                    dataset,
                    cancellation,
                    deadline,
                )?;
            } else {
                input.project_symbols(symbols, dataset, cancellation, deadline)?;
            }
        }
    }
    Ok(())
}

fn project_semantic_inventory(
    reads: &rift_server::ReadService,
    input: &rift_server::LocalSymbolViewInput,
    acquired: &AcquiredAdjacency,
    required: SemanticInventory,
    dataset: &mut rift_search::sparql::GraphDatasetBuilder,
    cancellation: &QueryCancellation,
    deadline: RequestDeadline,
) -> Result<InventoryCoverage, RiftError> {
    let coverage = if required.symbols {
        input.project_symbols(
            rift_search::sparql::SymbolSelection::All,
            dataset,
            cancellation,
            deadline.at().into_std(),
        )?
    } else {
        InventoryCoverage::Complete
    };
    acquired.project_captured_inventory(dataset, input, reads, cancellation, deadline)?;
    acquired.project_local(
        dataset,
        input,
        rift_search::sparql::SemanticEdges {
            relations: required.edges.relations
                || required.adjacency.references
                || required.adjacency.calls,
            ..required.edges
        },
        required.occurrences,
    )?;
    Ok(if acquired.is_partial() {
        InventoryCoverage::Partial
    } else {
        coverage
    })
}

/// Whether a symbol selection reads the captured inventory rather than one identity.
const fn reads_inventory(selection: rift_search::sparql::SymbolSelection<'_>) -> bool {
    !matches!(
        selection,
        rift_search::sparql::SymbolSelection::Identified(_)
    )
}

/// What a projection that reads the captured inventory, or one exact selection, reads from.
#[derive(Clone, Copy)]
struct InventoryProjection<'read> {
    query: &'read GraphQuery,
    input: &'read rift_server::LocalSymbolViewInput,
    reads: &'read rift_server::ReadService,
    acquired: &'read AcquiredAdjacency,
    local_semantics: Option<SemanticInventory>,
    exact: Option<&'read exact::ExactFacts>,
}

/// Projects the local facts a query without an adjacency selection reads. Returns the
/// inventory's coverage when the projection read the captured inventory rather than one
/// exact selection or identity.
fn project_inventory(
    projection: InventoryProjection<'_>,
    dataset: &mut rift_search::sparql::GraphDatasetBuilder,
    cancellation: &QueryCancellation,
    deadline: RequestDeadline,
) -> Result<Option<InventoryCoverage>, RiftError> {
    let InventoryProjection {
        query,
        input,
        reads,
        acquired,
        local_semantics,
        exact,
    } = projection;
    if let Some(required) = local_semantics {
        return project_semantic_inventory(
            reads,
            input,
            acquired,
            required,
            dataset,
            cancellation,
            deadline,
        )
        .map(Some);
    }
    if let Some(exact) = exact {
        exact.project(dataset)?;
        return Ok(None);
    }
    if let Some(selection) = query.history_selection() {
        input.project_history(
            reads,
            selection,
            dataset,
            cancellation,
            deadline.at().into_std(),
        )?;
        return Ok(reads_inventory(selection).then_some(InventoryCoverage::Complete));
    }
    if let Some(selection) = query.syntax_element_selection() {
        let coverage = input.project_syntax_elements(
            reads,
            selection,
            dataset,
            cancellation,
            deadline.at().into_std(),
        )?;
        return Ok(reads_inventory(selection.symbols).then_some(coverage));
    }
    if let Some(selection) = query.symbol_selection() {
        let coverage =
            input.project_symbols(selection, dataset, cancellation, deadline.at().into_std())?;
        return Ok(reads_inventory(selection).then_some(coverage));
    }
    Ok(None)
}

/// Whether an answer claims an absence: no rows, no triples, or a false `ASK`.
fn claims_absence(answer: &QueryAnswer) -> bool {
    match answer {
        QueryAnswer::Bindings { bindings, .. } => bindings.is_empty(),
        QueryAnswer::Graph { triples } => triples.is_empty(),
        QueryAnswer::Boolean { value } => !value,
    }
}

fn constrain_provider_expiry<'view>(
    view: &mut CapturedView,
    providers: impl Iterator<Item = &'view CapturedView>,
) -> Result<bool, RiftError> {
    let now = SystemTime::now();
    let mut expiry = crate::proxy::captured_expiry(view)?;
    for provider in providers {
        let selected = crate::proxy::captured_expiry(provider)?;
        if selected <= now {
            return Ok(false);
        }
        if selected < expiry {
            view.expires_at.clone_from(&provider.expires_at);
            expiry = selected;
        }
    }
    Ok(true)
}

impl AcquiredGraph {
    fn evaluate(
        self,
        query: &GraphQuery,
        deadline: RequestDeadline,
        cancellation: &QueryCancellation,
        views: &std::sync::OnceLock<rift_server::LocalSymbolViews>,
        paging: (u64, u64, rift_protocol::query::QueryInclude),
        key: [u8; 32],
    ) -> Result<QueryOutput, RiftError> {
        let Self {
            limits,
            reads,
            selected,
            local_semantics,
            acquired,
            exact,
            mut retrieved,
            configuration,
            mut warnings,
        } = self;
        if acquired.is_incomplete() {
            warnings.push(ReadWarning::EdgeCoverageMissing {
                kinds: Vec::new(),
                detail: "the selected edge read is incomplete, so edges outside its captured and engine coverage are missing from this answer".to_owned(),
            });
            if !query.admits_partial_rows() && !query.admits_partial_witness() {
                return Ok(insufficient_coverage(warnings).into());
            }
        }
        let input = reads.local_search_view_input(true)?;
        let mut dataset = GraphDataset::builder(limits);
        let mut inventory = InventoryCoverage::Complete;
        if let Some(selected) = &selected {
            if let Err(error) = acquired.project(&mut dataset, selected, &input) {
                if lacks_coverage(&error) {
                    return Ok(
                        query_unavailable(SymbolUnavailableReason::InsufficientCoverage).into(),
                    );
                }
                return Err(error);
            }
            project_local_endpoints(
                &input,
                &reads,
                &acquired,
                selected,
                &mut dataset,
                cancellation,
                deadline.at().into_std(),
            )?;
        } else {
            let inventory_read = project_inventory(
                InventoryProjection {
                    query,
                    input: &input,
                    reads: &reads,
                    acquired: &acquired,
                    local_semantics,
                    exact: exact.as_ref(),
                },
                &mut dataset,
                cancellation,
                deadline,
            )?;
            if let Some(read) = inventory_read {
                inventory = read;
                for warning in input.warnings()? {
                    if !warnings.contains(&warning) {
                        warnings.push(warning);
                    }
                }
            }
        }
        let coverage = project_retrievals(query, &mut retrieved, &mut dataset, &mut warnings)?;
        if coverage == ProjectedRetrievals::Unavailable {
            return Ok(insufficient_coverage(warnings).into());
        }
        let partial = acquired.leaves_facts_out()
            || coverage == ProjectedRetrievals::Partial
            || inventory == InventoryCoverage::Partial;
        if partial && !query.admits_partial_rows() && !query.admits_partial_witness() {
            return Ok(insufficient_coverage(warnings).into());
        }
        if query.names_local_graph() {
            if selected.is_some() {
                return Ok(query_unavailable(SymbolUnavailableReason::InsufficientCoverage).into());
            }
            dataset.publish_local_graph()?;
        }
        let Some(answer) =
            evaluate_acquired_query(&dataset.build(), query, cancellation, &acquired)?
        else {
            return Ok(insufficient_coverage(warnings).into());
        };
        // Over partial coverage an empty answer would claim an absence the missing facts may
        // contradict, so it refuses with the warnings that name what is missing.
        if partial && claims_absence(&answer) {
            return Ok(insufficient_coverage(warnings).into());
        }
        let views = views.get().ok_or_else(retention_failure)?;
        let retention = QueryRetention {
            key,
            answer: &answer,
            configuration: &configuration,
            paging,
            warnings,
            display: QueryDisplay::selected(&answer, query.presentation()),
            acquired: &acquired,
            retrieved: &retrieved,
        };
        retention.store(views, input, cancellation)
    }
}

struct QueryRetention<'answer> {
    key: [u8; 32],
    answer: &'answer QueryAnswer,
    configuration: &'answer QueryConfiguration,
    paging: (u64, u64, rift_protocol::query::QueryInclude),
    warnings: Vec<ReadWarning>,
    display: QueryDisplay,
    acquired: &'answer AcquiredAdjacency,
    retrieved: &'answer [AcquiredRetrieval],
}

impl QueryRetention<'_> {
    fn store(
        self,
        views: &rift_server::LocalSymbolViews,
        input: rift_server::LocalSymbolViewInput,
        cancellation: &QueryCancellation,
    ) -> Result<QueryOutput, RiftError> {
        let bytes = serde_json::to_vec(&(self.answer, &self.warnings, &self.display))
            .map_err(encoding_failure)?;
        let expires = self
            .provider_views()
            .map(crate::proxy::captured_expiry)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .min();
        let input = input.with_query(self.key, bytes, expires);
        let mut response = None;
        let limits = rift_server::RetainedViewLimits::from(self.configuration);
        views.capture_with(input, limits, |view| {
            let mut view = view.clone();
            if !constrain_provider_expiry(&mut view, self.provider_views())? {
                response = Some(query_unavailable(SymbolUnavailableReason::ViewExpired).into());
                return Ok(false);
            }
            if cancellation.is_cancelled() {
                return errors::search::query_cancelled().fail();
            }
            response = Some(query_page(
                &view,
                self.answer,
                QueryPageOptions {
                    include: self.paging.2,
                    configuration: self.configuration,
                    page: self.paging.0,
                    max_results: self.paging.1,
                    warnings: self.warnings,
                    display: self.display,
                },
            )?);
            Ok(true)
        })?;
        response.ok_or_else(retention_failure)
    }

    fn provider_views(&self) -> impl Iterator<Item = &CapturedView> {
        self.acquired.remote_view().into_iter().chain(
            self.retrieved
                .iter()
                .filter_map(|entry| entry.result.view.as_ref()),
        )
    }
}

fn query_key(
    query: &GraphQuery,
    language: rift_protocol::query::QueryLanguage,
    revision: &rift_protocol::revision::Revision,
    epoch: u64,
) -> Result<[u8; 32], RiftError> {
    let selected = query
        .retrieval_requests()
        .iter()
        .map(|selection| &selection.request)
        .collect::<Vec<_>>();
    // Publication movement invalidates dependency and configuration selections even
    // when a git commit remains the served revision.
    let bytes = serde_json::to_vec(&(query.normalized_text(), language, revision, epoch, selected))
        .map_err(encoding_failure)?;
    Ok(Sha256::digest(bytes).into())
}

fn evaluate_acquired_query(
    dataset: &GraphDataset,
    query: &GraphQuery,
    cancellation: &QueryCancellation,
    acquired: &AcquiredAdjacency,
) -> Result<Option<QueryAnswer>, RiftError> {
    let mut answer: QueryAnswer = dataset.evaluate(query, cancellation)?.into();
    dataset.annotate_providers(&mut answer)?;
    Ok((!acquired.is_incomplete()
        || matches!(answer, QueryAnswer::Boolean { value: true })
        || (query.admits_partial_rows() && !claims_absence(&answer)))
    .then_some(answer))
}

/// Selects one fixed-size answer page within the accepted response byte bound.
pub(crate) fn query_page(
    view: &CapturedView,
    answer: &QueryAnswer,
    options: QueryPageOptions<'_>,
) -> Result<QueryOutput, RiftError> {
    let provider = rift_search::sparql::VocabularyProperty::Provider.iri();
    let visible = |triple: &&rift_protocol::query::RdfTriple| {
        !matches!(&triple.predicate,
        rift_protocol::query::RdfTerm::Iri { value } if value == provider.as_str())
    };
    let total = match answer {
        QueryAnswer::Bindings { bindings, .. } => bindings.len(),
        QueryAnswer::Graph { triples } => triples.iter().filter(visible).count(),
        QueryAnswer::Boolean { .. } => 1,
    };
    let pagination = Pagination::new(options.page, options.max_results, total as u64);
    let paged = !matches!(answer, QueryAnswer::Boolean { .. });
    let start = pagination.offset().min(total);
    let end = start
        .saturating_add(usize::try_from(options.max_results).unwrap_or(usize::MAX))
        .min(total);
    let answer = match answer {
        QueryAnswer::Bindings {
            variables,
            bindings,
        } => QueryAnswer::Bindings {
            variables: variables.clone(),
            bindings: bindings[start..end].to_vec(),
        },
        QueryAnswer::Graph { triples } => QueryAnswer::Graph {
            triples: triples
                .iter()
                .filter(visible)
                .skip(start)
                .take(end - start)
                .cloned()
                .collect(),
        },
        QueryAnswer::Boolean { value } => QueryAnswer::Boolean { value: *value },
    };
    let answer = answer.project(options.include);
    let output = QueryOutput::new(
        QueryReadResult::Results(Box::new(QueryResult {
            answer,
            view: Some(view.clone()),
            revision: None,
            pagination: paged.then_some(pagination),
            warnings: options.warnings,
        })),
        options.display,
    );
    let maximum =
        usize::try_from(options.configuration.page_limit.bytes()).map_err(|_| page_failure())?;
    crate::proxy::encode_bounded(&output, maximum).map_err(|_| page_failure())?;
    Ok(output)
}
fn deadline_failure(budget: std::time::Duration) -> RiftError {
    errors::search::query_deadline_exceeded()
        .timeout_ms(u64::try_from(budget.as_millis()).unwrap_or(u64::MAX))
        .error()
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}

fn page_failure() -> RiftError {
    errors::server::read_invalid()
        .field("search.query.page_limit")
        .violation("one query row exceeds the accepted page limit; select fewer or smaller fields")
        .error()
}
fn retention_failure() -> RiftError {
    errors::server::read_unavailable()
        .operation("query retention")
        .detail("retained query material is unavailable; repeat the original query")
        .error()
}
fn lacks_coverage(error: &RiftError) -> bool {
    matches!(
        error.slug(),
        errors::server::search_coverage_unavailable::SLUG | errors::server::read_unsupported::SLUG
    )
}

fn unavailable(reason: SymbolUnavailableReason) -> SearchUnavailable {
    SearchUnavailable::Unavailable {
        reason,
        view: None,
        warnings: Vec::new(),
    }
}

fn query_unavailable(reason: SymbolUnavailableReason) -> QueryReadResult {
    QueryReadResult::Unavailable(unavailable(reason))
}

fn insufficient_coverage(warnings: Vec<ReadWarning>) -> QueryReadResult {
    QueryReadResult::Unavailable(coverage_unavailable(warnings))
}

fn coverage_unavailable(warnings: Vec<ReadWarning>) -> SearchUnavailable {
    SearchUnavailable::Unavailable {
        reason: SymbolUnavailableReason::InsufficientCoverage,
        view: None,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::query::QueryLanguage;
    use serde_json::{Value, json};

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    pub(super) mod dependency_inventory;

    #[test]
    fn query_pages_project_requested_providers_before_counting_graph_rows() -> TestResult {
        use rift_protocol::query::{QueryInclude, RdfTerm, RdfTriple};
        use rift_protocol::read::EdgeProvider;
        let providers = [
            EdgeProvider::Engine,
            EdgeProvider::Lsp,
            EdgeProvider::Static,
        ]
        .into_iter()
        .collect();
        let edge = RdfTerm::BlankNode {
            value: "edge".to_owned(),
            providers,
        };
        let triple = |predicate: &str, object| RdfTriple {
            subject: edge.clone(),
            predicate: RdfTerm::Iri {
                value: format!("urn:rift:vocabulary:{predicate}"),
            },
            object,
        };
        let mut triples = vec![triple(
            "to",
            RdfTerm::Iri {
                value: "rift://symbol/local/rust/app/callee".to_owned(),
            },
        )];
        triples.extend(["engine", "lsp", "static"].map(|provider| {
            triple(
                "provider",
                RdfTerm::Literal {
                    value: provider.to_owned(),
                    datatype: "http://www.w3.org/2001/XMLSchema#string".to_owned(),
                },
            )
        }));
        let answer = QueryAnswer::Graph { triples };
        let view = CapturedView {
            id: rift_protocol::symbol_read::CapturedViewId::parse(&"a".repeat(64))?,
            expires_at: "2099-01-01T00:00:00Z".to_owned(),
        };
        for included in [false, true] {
            for page in [1, 2] {
                let output = query_page(
                    &view,
                    &answer,
                    QueryPageOptions {
                        include: QueryInclude {
                            providers: included,
                        },
                        configuration: &QueryConfiguration::default(),
                        page,
                        max_results: 1,
                        warnings: Vec::new(),
                        display: QueryDisplay::default(),
                    },
                )?;
                let QueryReadResult::Results(result) = output.result else {
                    panic!("paged graph")
                };
                assert_eq!(result.pagination, Some(Pagination::new(page, 1, 1)));
                let QueryAnswer::Graph { triples } = &result.answer else {
                    panic!("graph answer")
                };
                assert_eq!(triples.len(), usize::from(page == 1));
                if page == 1 {
                    let value = serde_json::to_value(&triples[0])?;
                    assert!(
                        value["predicate"]["value"]
                            .as_str()
                            .is_some_and(|value| !value.ends_with(":provider"))
                    );
                    assert_eq!(
                        value["subject"].get("providers"),
                        included
                            .then(|| json!(["engine", "lsp", "static"]))
                            .as_ref()
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn query_key_names_normalized_text_revision_and_selection() -> TestResult {
        use rift_protocol::read::SearchScope;
        use rift_protocol::revision::Revision;
        let limits = GraphLimits::default();
        let revision = Revision::tree(&"a".repeat(64))?;
        let statement = "SELECT ?hit { SERVICE :search { ?hit :query 'beacon' } }";
        let original =
            GraphQuery::parse(statement, &limits)?.with_default_scope(&SearchScope::Local);
        let spaced = GraphQuery::parse(&format!("  {statement}\n# page"), &limits)?
            .with_default_scope(&SearchScope::Local);
        let key = query_key(&original, QueryLanguage::Sparql, &revision, 1)?;
        assert_eq!(
            key,
            query_key(&spaced, QueryLanguage::Sparql, &revision, 1)?
        );
        let global = original.clone().with_default_scope(&SearchScope::Global);
        assert_ne!(
            key,
            query_key(&global, QueryLanguage::Sparql, &revision, 1)?
        );
        assert_ne!(
            key,
            query_key(&original, QueryLanguage::Sparql, &revision, 2)?
        );
        let moved = Revision::tree(&"b".repeat(64))?;
        assert_ne!(key, query_key(&original, QueryLanguage::Sparql, &moved, 1)?);
        let package = |name| {
            GraphQuery::parse(
                &format!(
                    "SELECT ?hit {{ SERVICE :search {{ ?hit :query 'beacon'; :scope 'global'; :packages '[{{\"manager\":\"cargo\",\"registry\":\"crates.io\",\"name\":\"{name}\"}}]'^^rdf:JSON }} }}"
                ),
                &limits,
            )
        };
        let serde = package("serde")?;
        let tokio = package("tokio")?;
        assert_ne!(
            query_key(&serde, QueryLanguage::Sparql, &revision, 1)?,
            query_key(&tokio, QueryLanguage::Sparql, &revision, 1)?
        );
        Ok(())
    }

    #[tokio::test]
    async fn occurrence_inventory_does_not_project_unrequested_symbols_and_edges() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(
            directory.path().join("entry.ts"),
            "interface Value {}\ntype Alias = Value;\nconst unused = { first: 1, second: 2, third: 3 };\n",
        )?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let limits = GraphLimits::builder().triples_max(16).build()?;
        let query = GraphQuery::parse(
            "SELECT (COUNT(*) AS ?n) { ?target :usages ?usage }",
            &limits,
        )?;
        let deadline = server.request_deadline().await;
        let resolved = server
            .published_workspace(super::super::super::wire::ErrorPhase::Read, deadline)
            .await
            .map_err(|failure| format!("{failure:?}"))?;
        let revision = resolved.published.reads.served_revision()?;
        let epoch = resolved.published.epoch;
        let key = query_key(&query, QueryLanguage::Sparql, &revision, epoch)?;
        let graph = server
            .prepare_graph(resolved, &query, limits, deadline)
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .map_err(|reason| format!("{reason:?}"))?;
        let output = graph.evaluate(
            &query,
            deadline,
            &QueryCancellation::new(),
            &server.local_views,
            (1, 10, rift_protocol::query::QueryInclude::default()),
            key,
        )?;
        assert!(
            matches!(&output.result, QueryReadResult::Results(result)
                if matches!(&result.answer, QueryAnswer::Bindings { bindings, .. }
                    if matches!(&bindings[0]["n"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "1"))),
            "{:?}",
            output.result
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_pages_select_bindings_at_the_requested_revision() -> TestResult {
        let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='graph_fixture'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
        )?;
        std::fs::write(
            directory.path().join("lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\n",
        )?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let mut params = QueryParams {
            query:
                "SELECT ?name { ?symbol a :Symbol; :name ?name; :kind 'function' } ORDER BY ?name"
                    .to_owned(),
            max_results: 1,
            ..Default::default()
        };
        for page in 1..=3 {
            params.page = page;
            let QueryReadResult::Results(answer) = server
                .graph_query(params.clone())
                .await
                .map_err(|error| format!("{error:?}"))?
                .0
                .result
            else {
                return Err("query result".into());
            };
            assert_eq!(answer.pagination, Some(Pagination::new(page, 1, 2)));
            params.revision = answer.revision.clone();
            if page == 1 {
                params.query = format!("  {}\n# page", params.query);
            }
            let QueryAnswer::Bindings { bindings, .. } = answer.answer else {
                return Err("bindings".into());
            };
            assert_eq!(bindings.len(), usize::from(page <= 2));
        }
        let evaluations = drain
            .queued_records()
            .iter()
            .filter(|record| {
                record.message() == "worker admitted"
                    && serde_json::from_str::<Value>(record.fields())
                        .is_ok_and(|fields| fields["work"] == "graph query")
            })
            .count();
        assert_eq!(evaluations, 1, "later pages read the retained answer");
        let previous_revision = params.revision.take();
        std::fs::write(
            root.join("lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n",
        )?;
        server.validation.observe_whole_workspace()?;
        params.page = 1;
        let QueryReadResult::Results(answer) = server
            .graph_query(params.clone())
            .await
            .map_err(|error| format!("{error:?}"))?
            .0
            .result
        else {
            return Err("query result".into());
        };
        assert_ne!(answer.revision, previous_revision);
        assert_eq!(answer.pagination, Some(Pagination::new(1, 1, 3)));
        params.revision = answer.revision;
        params.page = 2;
        server
            .graph_query(params)
            .await
            .map_err(|error| format!("{error:?}"))?;
        let evaluations = drain
            .queued_records()
            .iter()
            .filter(|record| {
                record.message() == "worker admitted"
                    && serde_json::from_str::<Value>(record.fields())
                        .is_ok_and(|fields| fields["work"] == "graph query")
            })
            .count();
        assert_eq!(evaluations, 1, "a moved revision evaluates once");
        Ok(())
    }

    #[tokio::test]
    async fn repeated_query_pages_do_not_exhaust_retained_views() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let mut request = QueryParams {
            query: "SELECT ?value { VALUES ?value { 1 2 } } ORDER BY ?value".to_owned(),
            max_results: 1,
            ..Default::default()
        };
        for page in 0..300 {
            request.page = page % 2 + 1;
            let output = server
                .graph_query(request.clone())
                .await
                .map_err(|error| format!("{error:?}"))?
                .0;
            let QueryReadResult::Results(answer) = output.result else {
                return Err("query result".into());
            };
            assert_eq!(answer.pagination, Some(Pagination::new(request.page, 1, 2)));
            request.revision = answer.revision.clone();
        }
        Ok(())
    }

    #[tokio::test]
    async fn distinct_queries_past_the_view_bound_release_and_evaluate_again() -> TestResult {
        let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(
            directory.path(),
            "[search.query]\nretained_views = 2\n",
        )?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let query = |first: u64| QueryParams {
            query: format!(
                "SELECT ?value {{ VALUES ?value {{ {first} {} }} }} ORDER BY ?value",
                first + 1
            ),
            max_results: 1,
            ..Default::default()
        };
        let read = |request: QueryParams| {
            let server = server.clone();
            async move {
                let output = server
                    .graph_query(request)
                    .await
                    .map_err(|error| format!("{error:?}"))?
                    .0;
                match output.result {
                    QueryReadResult::Results(answer) => Ok::<_, String>(answer),
                    QueryReadResult::Unavailable(unavailable) => Err(format!("{unavailable:?}")),
                }
            }
        };
        let mut evaluations = move || {
            drain
                .queued_records()
                .iter()
                .filter(|record| {
                    record.message() == "worker admitted"
                        && serde_json::from_str::<Value>(record.fields())
                            .is_ok_and(|fields| fields["work"] == "graph query")
                })
                .count()
        };
        let first = read(query(0)).await?;
        for distinct in 1..=5 {
            let answer = read(query(distinct * 10)).await?;
            assert_eq!(answer.pagination, Some(Pagination::new(1, 1, 2)));
        }
        assert_eq!(
            evaluations(),
            6,
            "every distinct query past the bound is answered"
        );
        let mut released = query(0);
        released.page = 2;
        released.revision = first.revision.clone();
        let answer = read(released).await?;
        assert_eq!(answer.pagination, Some(Pagination::new(2, 1, 2)));
        assert_eq!(
            evaluations(),
            1,
            "a later page of a released view evaluates the query again"
        );
        let mut retained = query(50);
        retained.page = 2;
        retained.revision = first.revision;
        let answer = read(retained).await?;
        assert_eq!(answer.pagination, Some(Pagination::new(2, 1, 2)));
        assert_eq!(
            evaluations(),
            0,
            "a retained view still answers its later page"
        );
        Ok(())
    }

    #[tokio::test]
    async fn graph_query_acquires_complete_local_semantics_for_general_algebra() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(
            directory.path(),
            "[global]\nenabled = false\n[languages.toml]\nenabled = false\n[languages.python]\nstdlib = false\n[languages.python.lsp]\nembedded = 'ty'\n",
        )?;
        std::fs::write(
            directory.path().join("pyproject.toml"),
            "[project]\nname = 'graph-fixture'\nversion = '0.1.0'\n[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['helper','main']\n",
        )?;
        std::fs::write(
            directory.path().join("helper.py"),
            "def beacon() -> int:\n    return other()\n\ndef other() -> int:\n    return 0\n",
        )?;
        std::fs::write(
            directory.path().join("main.py"),
            "from helper import beacon\n\nbeacon()\n\ndef caller() -> int:\n    return beacon() + beacon()\n\ncaller()\n",
        )?;
        let root = directory.path().canonicalize()?;
        dependency_inventory::observed_python_environment(&root)?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let count = server
            .graph_query(QueryParams {
                query: "SELECT ?name (COUNT(?usage) AS ?n) FROM <urn:rift:graph:local> { \
                        ?symbol :name ?name; :usages ?usage . FILTER (?name = 'beacon') \
                        } GROUP BY ?name"
                    .to_owned(),
                language: QueryLanguage::Sparql,
                ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        assert!(
            matches!(&count, QueryReadResult::Results(result)
                if matches!(&result.answer, QueryAnswer::Bindings { bindings, .. }
                    if bindings.len() == 1
                        && matches!(&bindings[0]["n"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "4"))),
            "{count:?}"
        );

        let negative = server
            .graph_query(QueryParams {
                query: "ASK { ?caller a :Symbol; :name 'caller' . \
                        FILTER NOT EXISTS { ?caller :references ?target . ?target :name 'other' } }"
                    .to_owned(),
                language: QueryLanguage::Sparql,
                ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        assert!(
            matches!(&negative, QueryReadResult::Results(result)
                if matches!(&result.answer, QueryAnswer::Boolean { value: true })),
            "{negative:?}"
        );
        let mixed = server
            .graph_query(QueryParams {
                query: "SELECT DISTINCT ?name { ?start :calls/:references ?end; :name 'caller' . ?end :name ?name }"
                    .to_owned(),
                language: QueryLanguage::Sparql,
            ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0.result;
        assert!(
            matches!(&mixed, QueryReadResult::Results(result)
                if matches!(&result.answer, QueryAnswer::Bindings { bindings, .. }
                    if bindings.len() == 1
                        && matches!(&bindings[0]["name"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "other"))),
            "{mixed:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn graph_query_keeps_each_local_usage_and_its_exact_source() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(
            directory.path(),
            "[languages.python.lsp]\nembedded = 'ty'\n",
        )?;
        std::fs::write(
            directory.path().join("pyproject.toml"),
            "[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['helper','main']\n",
        )?;
        std::fs::write(
            directory.path().join("helper.py"),
            "def beacon() -> int:\n    return 7\n",
        )?;
        std::fs::write(
            directory.path().join("main.py"),
            "from helper import beacon\n\nresult = beacon()\n\ndef caller() -> int:\n    return beacon() + beacon()\n",
        )?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let published = Arc::clone(&server.published.read().await.current);
        let params = serde_json::from_value(serde_json::json!({"name": "beacon"}))?;
        let id = published
            .reads
            .get_symbol(&params)?
            .hits
            .into_iter()
            .find_map(|hit| hit.symbol.id)
            .ok_or("canonical beacon declaration")?;
        let params = QueryParams {
            query: format!(
                "SELECT ?start ?text {{ <{}> :usages ?usage . ?usage :range/:start ?start ; :source ?text . FILTER (?text = 'beacon') }} ORDER BY DESC(?start + 1)",
                id.as_str()
            ),
            ..Default::default()
        };
        let result = server
            .graph_query(params)
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        let QueryReadResult::Results(result) = result else {
            return Err(format!("expected captured usages, received {result:?}").into());
        };
        let QueryAnswer::Bindings { bindings, .. } = result.answer else {
            return Err("expected usage bindings".into());
        };
        assert_eq!(bindings.len(), 4, "{bindings:?}");
        assert!(bindings.iter().all(|binding| matches!(
            &binding["text"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "beacon"
        )));
        let count = server
            .graph_query(QueryParams {
                query: format!(
                    "SELECT (COUNT(?usage) AS ?n) {{ <{}> :usages ?usage }}",
                    id.as_str()
                ),
                ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        assert!(
            matches!(count, QueryReadResult::Results(ref result)
            if matches!(&result.answer, QueryAnswer::Bindings { bindings, .. }
                if matches!(&bindings[0]["n"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "4"))),
            "{count:?}"
        );

        let metadata = server
            .graph_query(QueryParams {
                query: format!(
                    "SELECT DISTINCT ?name {{ ?caller :references <{}>; :name ?name }}",
                    id.as_str()
                ),
                ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        assert!(
            matches!(metadata, QueryReadResult::Results(ref result)
            if matches!(&result.answer, QueryAnswer::Bindings { bindings, .. }
                if bindings.iter().any(|row| matches!(&row["name"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "caller")))),
            "{metadata:?}"
        );
        let evidence = server.graph_query(QueryParams {
            query: format!("SELECT ?source ?unit {{ ?syntax_element :of <{}> ; :index 0 ; :source ?source ; :range [ :unit ?unit ; :start ?start ; :end ?end ] }}", id.as_str()),
        ..Default::default()
        }).await.map_err(|failure| format!("{failure:?}"))?.0.result;
        assert!(
            matches!(evidence, QueryReadResult::Results(ref result)
            if matches!(&result.answer, QueryAnswer::Bindings { bindings, .. }
                if matches!(&bindings[0]["unit"], rift_protocol::query::RdfTerm::Iri { value } if value == "rift://source/project/helper.py")
                && matches!(&bindings[0]["source"], rift_protocol::query::RdfTerm::Literal { value, .. } if value.contains("return 7")))),
            "{evidence:?}"
        );
        Ok(())
    }

    const ROOT: &str = "rift://symbol/cargo/crates.io/demo/rust/root";
    const A: &str = "rift://symbol/cargo/crates.io/demo/rust/a";
    const B: &str = "rift://symbol/cargo/crates.io/dependency/rust/b";
    const C: &str = "rift://symbol/cargo/crates.io/dependency/rust/c";
    const VIEW: &str = "7acfa78d70b3a8211b44149026dbab4c5f28bbf93240d7817235d3722481069d";

    fn json_response(value: &Value) -> impl axum::response::IntoResponse + use<> {
        ([("content-type", "application/json")], value.to_string())
    }

    fn graph_capabilities(managers: &[&str], edges: &[Value]) -> Value {
        json!({
            "supported_package_managers": managers,
                    "supported_features": ["resolutions", "search", "symbols", "edges", "facets"],
                    "publication_format": "rift-package-index-v2",
                    "analyzer_revision": "analyzer-v1",
                    "corpus_revision": "corpus-v1",
                    "required_search_fields": ["name","qualified_name","documentation","signature"],
                    "bounds": {
                        "request_body_bytes_max": 1_048_576, "response_body_bytes_max": 1_048_576,
                        "dependency_entries_max": 100, "query_bytes_max": 4096, "query_terms_max": 32,
                        "query_term_bytes_max": 256, "identifiers_max": 16, "identifier_bytes_max": 4096,
                        "packages_max": 100, "page_limit_min": 1, "page_limit_max": 100,
                        "page_limit_default": 10, "candidate_pool_max": 100,
                        "warnings_max": 32, "source_bytes_max": 1_048_576
                    },
                    "edge_capabilities": edges
        })
    }

    fn graph_adjacency_capability(direction: &str, kinds: &[&str]) -> Value {
        json!({
            "direction": direction, "kinds": kinds, "bounds": {
                "max_seeds": 8, "max_kinds": 4, "max_edges": 100,
                "max_occurrences": 100, "max_symbols": 100, "max_sources": 100
            }
        })
    }

    fn graph_fixture_symbol(id: &str, source: bool) -> (Value, Option<Value>) {
        let name = id.rsplit('/').next().expect("name in symbol identity");
        if !source {
            return (
                json!({
                    "symbol":{"id":id,"name":name,"language":"rust","kind":"function",
                        "origin":{"source_kind":"synthetic"}},
                    "declarations":[]
                }),
                None,
            );
        }
        let (package, version) = if id == ROOT || id == A {
            ("demo", "1.0.0")
        } else {
            ("dependency", "2.0.0")
        };
        let origin = json!({
            "source_kind":"authored","location":"dependency",
            "package":{"manager":"cargo","registry":"crates.io","name":package,"version":version}
        });
        let prefix = "// header\n";
        let text = format!("{prefix}fn {name}() {{ \"café\"; }}");
        let unit = format!("rift://source/cargo/crates.io/{package}/src/{name}.rs");
        let digest =
            data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(text.as_bytes()).as_bytes());
        (
            json!({
                "symbol":{"id":id,"name":name,"language":"rust","kind":"function","origin":origin},
                "declarations":[{
                    "origin":origin,"unit":unit,"range":{"start":prefix.len(),"end":text.len()},"line":2,
                    "signature_indices":[],"type_indices":[],"documentation_indices":[]
                }]
            }),
            Some(json!({"unit":unit,"digest":digest,"text":text})),
        )
    }

    fn remote_graph_routes(
        requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
        endpoint_expiry: String,
    ) -> axum::Router {
        use axum::routing::{get, post};

        axum::Router::new()
            .route("/rift/v1/capabilities", get(|| async {
                json_response(&graph_capabilities(
                    &["cargo"],
                    &[graph_adjacency_capability("outgoing", &["calls", "references"])],
                ))
            }))
            .route("/rift/v1/search", post(move |bytes: axum::body::Bytes| {
                let requests = Arc::clone(&requests);
                let expires = endpoint_expiry.clone();
                async move {
                    let request: Value = serde_json::from_slice(&bytes).expect("valid raw edge request");
                    let facet = request["edges"]["kinds"][0].as_str().unwrap_or_default();
                    let seed = request["edges"]["seeds"][0].as_str().unwrap_or_default();
                    let (next, number) = match seed {
                        ROOT => (A, 1_u64),
                        A => (B, 2),
                        B => (C, 3),
                        _ => return json_response(&json!({"outcome":"unavailable","reason":"insufficient_coverage"})),
                    };
                    let source = request["include"]["source"] == json!(true);
                    let (seed_symbol, seed_source) = graph_fixture_symbol(seed, source);
                    let (next_symbol, next_source) = graph_fixture_symbol(next, source);
                    let sources = seed_source.into_iter().chain(next_source).collect::<Vec<_>>();
                    let result = json!({
                        "groups": [{"seed":seed,"complete":true,"edges":[{
                            "id": format!("{number:064x}"),
                            "edge":{"from":seed,"to":next,"kind":facet,"kinds":[facet],"providers": ["static"], "derivation":"resolution"}
                        }]}],
                        "symbols":[seed_symbol,next_symbol],
                        "sources":sources,
                        "analyzer_revision":"analyzer-v1","corpus_revision":"corpus-v1",
                        "view":{"id":VIEW,"expires_at":expires}
                    });
                    requests.lock().await.push(request);
                    json_response(&result)
                }
            }))
    }

    const PACKAGE_GREETING: &str =
        "rift://symbol/pypi/pypi.org/greeting/python/greeting/core/greet";
    const PACKAGE_BRIDGE: &str = "rift://symbol/pypi/pypi.org/otherpkg/python/otherpkg/bridge";
    const PACKAGE_ENTRY: &str = "rift://symbol/pypi/pypi.org/otherpkg/python/otherpkg/entry";
    const PACKAGE_OUTER: &str = "rift://symbol/pypi/pypi.org/otherpkg/python/otherpkg/outer";

    fn reverse_graph_routes() -> axum::Router {
        use axum::routing::{get, post};

        axum::Router::new()
            .route("/rift/v1/capabilities", get(|| async {
                json_response(&graph_capabilities(
                    &["pypi"],
                    &[graph_adjacency_capability("incoming", &["calls"])],
                ))
            }))
            .route("/rift/v1/search", post(|bytes: axum::body::Bytes| async move {
                let request: Value = serde_json::from_slice(&bytes).expect("valid reverse request");
                assert_eq!(request["edges"]["direction"], "incoming");
                assert_eq!(request["edges"]["kinds"], json!(["calls"]));
                assert_eq!(request["packages"].as_array().expect("exact domain").len(), 2);
                let (seed, caller, id) = match request["edges"]["seeds"][0].as_str() {
                    Some(PACKAGE_GREETING) => (PACKAGE_GREETING, PACKAGE_BRIDGE, 1),
                    Some(PACKAGE_BRIDGE) => (PACKAGE_BRIDGE, PACKAGE_ENTRY, 2),
                    Some(PACKAGE_ENTRY) => {
                        assert_eq!(request["view"], VIEW);
                        (PACKAGE_ENTRY, PACKAGE_OUTER, 3)
                    }
                    _ => return json_response(&json!({"outcome":"unavailable","reason":"insufficient_coverage"})),
                };
                let symbol = |id: &str| json!({
                    "symbol": {"id":id,"name":id.rsplit('/').next(),"language":"python",
                        "kind":"function","origin":{"source_kind":"synthetic"}}, "declarations":[]
                });
                json_response(&json!({
                    "groups": [{"seed":seed,"complete":true,"edges":[{
                        "id": format!("{id:064x}"),
                        "edge":{"from":caller,"to":seed,"kind":"calls",
                            "kinds":["calls"],"providers": ["static"], "derivation":"resolution"}
                    }]}],
                    "symbols":[symbol(seed),symbol(caller)],
                    "analyzer_revision":"analyzer-v1","corpus_revision":"corpus-v1",
                    "view":{"id":VIEW,"expires_at":"2099-01-01T00:00:00Z"}
                }))
            }))
    }

    fn reverse_graph_workspace(
        root: &std::path::Path,
        endpoint: std::net::SocketAddr,
    ) -> TestResult {
        crate::server::hermetic_workspace(
            root,
            &format!(
                "[source]\nexclude = ['.venv/**']\n\
                 [languages.toml]\nenabled = false\n\
                 [languages.python]\nstdlib = false\n\
                 [languages.python.lsp]\nembedded = 'ty'\n\
                 [global]\nenabled = true\nendpoint = 'http://{endpoint}/rift'\nattempts = 1\n"
            ),
        )?;
        let site_packages = root.join(if cfg!(windows) {
            ".venv/Lib/site-packages"
        } else {
            ".venv/lib/python3.12/site-packages"
        });
        let files = [
            (
                root.join("pyproject.toml"),
                "[project]\nname = 'app'\nversion = '0.1.0'\ndependencies = ['greeting', 'otherpkg']\n\
              [build-system]\nrequires = ['setuptools==80.9.0']\nbuild-backend = 'setuptools.build_meta'\n\
              [tool.setuptools]\npy-modules = ['app']\n",
            ),
            (
                root.join("uv.lock"),
                "version = 1\n[[package]]\nname = 'app'\nversion = '0.1.0'\nsource = {editable='.'}\n\
              [[package]]\nname = 'greeting'\nversion = '1.0.0'\nsource = {registry='https://pypi.org/simple'}\n\
              [[package]]\nname = 'otherpkg'\nversion = '2.0.0'\nsource = {registry='https://pypi.org/simple'}\n",
            ),
            (
                root.join("app.py"),
                "from greeting.core import greet\n\ndef hello() -> str:\n    return greet('rift')\n\ndef entry() -> str:\n    return hello()\n",
            ),
            (
                root.join(".venv/pyvenv.cfg"),
                "home=/usr/bin\nversion_info=3.12.4\n",
            ),
            (
                site_packages.join("greeting/__init__.py"),
                "from greeting.core import greet\n",
            ),
            (
                site_packages.join("greeting/core.py"),
                "def greet(name: str) -> str:\n    return name\n",
            ),
            (
                site_packages.join("greeting-1.0.0.dist-info/RECORD"),
                "greeting/__init__.py,,\ngreeting/core.py,,\ngreeting-1.0.0.dist-info/RECORD,,\n",
            ),
        ];
        for (path, source) in files {
            std::fs::create_dir_all(path.parent().ok_or("fixture file parent")?)?;
            std::fs::write(path, source)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn graph_query_combines_package_and_local_incoming_calls() -> TestResult {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let endpoint = listener.local_addr()?;
        let service =
            tokio::spawn(async move { axum::serve(listener, reverse_graph_routes()).await });
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        reverse_graph_workspace(&root, endpoint)?;
        let limits = rift_index::WorkspaceIndexLimits::default();
        let server = RiftMcp::build_settled(&root, limits).await?;
        let query = QueryParams {
            query: format!("SELECT DISTINCT ?caller {{ ?caller :calls <{PACKAGE_GREETING}> }}"),
            language: QueryLanguage::Sparql,
            ..Default::default()
        };
        let inverse = QueryParams {
            query: format!(
                "SELECT DISTINCT ?caller {{ <{PACKAGE_BRIDGE}> ^:calls/^:calls ?caller }}"
            ),
            language: QueryLanguage::Sparql,
            ..Default::default()
        };
        for (query, expected) in [
            (
                query.clone(),
                ["rift://symbol/local/python/app/hello", PACKAGE_BRIDGE].as_slice(),
            ),
            (inverse, [PACKAGE_OUTER].as_slice()),
        ] {
            let response = server
                .graph_query(query)
                .await
                .map_err(|failure| format!("{failure:?}"))?
                .0
                .result;
            let QueryReadResult::Results(result) = response else {
                return Err(
                    format!("expected complete local and package callers: {response:?}").into(),
                );
            };
            let QueryAnswer::Bindings { bindings, .. } = result.answer else {
                return Err("expected caller bindings".into());
            };
            let ids = bindings
                .iter()
                .filter_map(|row| match &row["caller"] {
                    rift_protocol::query::RdfTerm::Iri { value } => Some(value.as_str()),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(ids, expected.iter().copied().collect());
        }

        let witness = server
            .graph_query(QueryParams {
                query: format!("ASK {{ <{PACKAGE_GREETING}> ^:calls/^:calls ?caller }}"),
                language: QueryLanguage::Sparql,
                ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        assert!(
            matches!(
                &witness,
                QueryReadResult::Results(result)
                    if matches!(result.answer, QueryAnswer::Boolean { value: true })
            ),
            "{witness:?}"
        );

        std::fs::create_dir(root.join("invalid-directory"))?;
        std::fs::write(
            root.join("invalid-directory/unestablished.py"),
            "def might_call(): pass\n",
        )?;
        let partial = RiftMcp::build_settled(&root, limits).await?;
        let response = partial
            .graph_query(query)
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        assert_identified_incoming_callers(response)?;
        service.abort();
        Ok(())
    }

    fn assert_identified_incoming_callers(response: QueryReadResult) -> TestResult {
        let QueryReadResult::Results(answer) = response else {
            return Err(format!("identified callers remain available: {response:?}").into());
        };
        let expected = ["rift://symbol/local/python/app/hello", PACKAGE_BRIDGE];
        assert!(
            matches!(&answer.answer, QueryAnswer::Bindings { bindings, .. }
        if bindings.len() == expected.len() && bindings.iter().all(|row|
            matches!(&row["caller"], rift_protocol::query::RdfTerm::Iri { value }
                if expected.contains(&value.as_str())))),
            "{answer:?}"
        );
        assert!(
            answer.warnings.iter().any(
                |warning| matches!(warning, ReadWarning::EdgeCoverageMissing { detail, .. }
            if detail.contains("identities"))
            ),
            "{:?}",
            answer.warnings
        );
        Ok(())
    }

    #[tokio::test]
    async fn graph_query_walks_mixed_hops_across_exact_releases_in_one_captured_view() -> TestResult
    {
        let expires = jiff::Timestamp::now()
            .checked_add(jiff::Span::new().minutes(2))?
            .to_string();
        let requests_seen = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let app = remote_graph_routes(Arc::clone(&requests_seen), expires.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let endpoint = listener.local_addr()?;
        let service = tokio::spawn(async move { axum::serve(listener, app).await });
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        crate::server::hermetic_workspace(
            directory.path(),
            &format!(
                "[global]\nenabled = true\nendpoint = 'http://{endpoint}/rift'\nattempts = 1\n\n\
                 [[dependencies.packages]]\nmanager = 'cargo'\nregistry = 'crates.io'\nname = 'demo'\nversion = '1.0.0'\n\n\
                 [[dependencies.packages]]\nmanager = 'cargo'\nregistry = 'crates.io'\nname = 'dependency'\nversion = '2.0.0'\n"
            ),
        )?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let published = Arc::clone(&server.published.read().await.current);
        let identity = rift_protocol::identity::SymbolIdentity::parse(ROOT)?;
        assert!(
            crate::global::workspace_release(
                published.reads.dependency_context(),
                identity.owner()
            )
            .is_some(),
            "expected exact release in {:?}",
            published.reads.dependency_context().entries()
        );
        assert!(published.configuration.global_configuration().enabled);
        let query = format!("SELECT ?callee {{ <{ROOT}> :calls/:references/:calls ?callee }}");
        let graph = GraphQuery::parse(&query, &GraphLimits::default())?;
        assert!(graph.adjacency_selection().is_some(), "{graph:?}");
        let result = server
            .graph_query(QueryParams {
                query,
                language: QueryLanguage::Sparql,
                ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        service.abort();
        let QueryReadResult::Results(result) = result else {
            return Err(format!(
                "expected captured remote path, received {result:?} after {:?} requests",
                requests_seen.lock().await.as_slice()
            )
            .into());
        };
        let view = result.view.as_ref().ok_or("captured view")?;
        assert_eq!(
            view.expires_at.parse::<jiff::Timestamp>()?,
            expires.parse::<jiff::Timestamp>()?
        );
        let QueryAnswer::Bindings { bindings, .. } = result.answer else {
            return Err("expected call bindings".into());
        };
        assert_eq!(bindings.len(), 1, "{bindings:?}");
        assert!(
            matches!(&bindings[0]["callee"], rift_protocol::query::RdfTerm::Iri { value } if value == C)
        );
        let requests_seen = requests_seen.lock().await;
        assert_eq!(requests_seen[1]["edges"]["kinds"], json!(["references"]));
        assert_eq!(requests_seen.len(), 3);
        assert!(requests_seen[0].get("view").is_none());
        assert!(
            requests_seen[1..]
                .iter()
                .all(|request| request["view"] == VIEW)
        );
        assert!(requests_seen.iter().all(|request| request["packages"]
            == json!([
                {"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"},
                {"manager":"cargo","registry":"crates.io","name":"dependency","version":"2.0.0"}
            ])));
        Ok(())
    }

    #[tokio::test]
    async fn graph_query_reads_captured_declaration_excerpts_without_an_exact_syntax_element_read()
    -> TestResult {
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let app = remote_graph_routes(Arc::clone(&requests), "2099-01-01T00:00:00Z".to_owned());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let endpoint = listener.local_addr()?;
        let service = tokio::spawn(async move { axum::serve(listener, app).await });
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        crate::server::hermetic_workspace(
            &root,
            &format!(
                "[global]\nenabled = true\nendpoint = 'http://{endpoint}/rift'\nattempts = 1\n\n\
                 [[dependencies.packages]]\nmanager = 'cargo'\nregistry = 'crates.io'\nname = 'demo'\nversion = '1.0.0'\n"
            ),
        )?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let statement = format!(
            "SELECT ?source ?unit ?start ?end ?provider {{ <{ROOT}> :calls ?target . \
             ?edge a :Edge; :from <{ROOT}>; :to ?target; :provider ?provider . \
             ?syntax_element :of ?target; :index 0; :source ?source; :range [ :unit ?unit; :start ?start; :end ?end ] }}"
        );
        let response = server
            .graph_query(QueryParams {
                query: statement,
                language: QueryLanguage::Sparql,
                ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        let QueryReadResult::Results(result) = response else {
            return Err(
                format!("expected captured declaration excerpt, received {response:?}").into(),
            );
        };
        let QueryAnswer::Bindings { bindings, .. } = result.answer else {
            return Err("expected declaration bindings".into());
        };
        assert_eq!(bindings.len(), 1);
        let row = serde_json::to_value(&bindings[0])?;
        assert_eq!(row["source"]["value"], "fn a() { \"café\"; }");
        assert_eq!(
            row["unit"]["value"],
            "rift://source/cargo/crates.io/demo/src/a.rs"
        );
        assert_eq!(row["provider"]["value"], "static");
        assert_eq!(row["start"]["value"], "10");
        assert_eq!(row["end"]["value"], "29");

        let metadata = server.graph_query(QueryParams {
            query: format!("SELECT ?kind {{ <{ROOT}> :calls ?target . ?syntax_element :of ?target; :index 0; :kind ?kind }}"),
            language: QueryLanguage::Sparql,
        ..Default::default()
        }).await.map_err(|failure| format!("{failure:?}"))?.0.result;
        assert!(
            matches!(
                metadata,
                QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
                    reason: SymbolUnavailableReason::InsufficientCoverage,
                    ..
                })
            ),
            "{metadata:?}"
        );
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]["include"],
            json!({"source": true, "providers": true})
        );
        service.abort();
        Ok(())
    }

    #[tokio::test]
    async fn graph_query_reads_remote_syntax_element_pages_at_the_selected_release() -> TestResult {
        const RELEASE: &str = "rift://revision/cargo/crates.io/demo@1.0.0";
        const UNIT: &str = "rift://source/cargo/crates.io/demo/src/lib.rs";
        let pages = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&pages);
        let app = remote_graph_routes(
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            "2099-01-01T00:00:00Z".to_owned(),
        )
        .route("/rift/v1/symbols", axum::routing::post(|bytes: axum::body::Bytes| async move {
            let request: Value = serde_json::from_slice(&bytes).expect("exact symbol request");
            assert_eq!(request["id"], ROOT);
            assert_eq!(request["revision"], RELEASE);
            let origin = json!({
                "source_kind":"authored", "location":"dependency",
                "package":{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}
            });
            json_response(&json!({
                "outcome":"found", "revision":RELEASE, "pagination":{"page":1,"pages":1,"max_results":1,"total":1},
                "symbol":{"id":ROOT,"name":"root","language":"rust","kind":"function","origin":origin},
                "declarations":[{"origin":origin,"unit":UNIT,"range":{"start":0,"end":12},"line":1,
                    "signature_indices":[],"type_indices":[],"documentation_indices":[]}]
            }))
        }))
        .route("/rift/v1/facets", axum::routing::post(move |bytes: axum::body::Bytes| {
            let observed = Arc::clone(&observed);
            async move {
                let request: Value = serde_json::from_slice(&bytes).expect("exact syntax element request");
                assert_eq!(request["symbol"], ROOT);
                assert_eq!(request["revision"], RELEASE);
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let page_number = request["page"].as_u64().expect("page");
                let max_results = request["max_results"].as_u64().expect("max_results");
                let declarations = (0..2).map(|ordinal| {
                let start = ordinal * 13;
                let source = "fn root() {}";
                json!({
                    "declaration":ordinal,"kind":"function_item","range":{"start":start,"end":start+12},
                    "source":{"span":{"unit":UNIT,"range":{"start":start,"end":start+12}},"text":source},
                    "syntax":[
                        {"declaration":ordinal,"index":0,"unit":UNIT,"language":"rust","kind":"function_item","range":{"start":start,"end":start+12},"source":source},
                        {"declaration":ordinal,"index":1,"unit":UNIT,"language":"rust","kind":"identifier","range":{"start":start+3,"end":start+7},"source":"root","parent":0}
                    ]
                })
                }).collect::<Vec<_>>();
                let pagination = rift_protocol::read::Pagination::new(page_number, max_results, 2);
                let selected = declarations.into_iter().skip(pagination.offset()).take(usize::try_from(max_results).expect("fixture count")).collect::<Vec<_>>();
                let page = json!({"symbol":ROOT,"revision":RELEASE,"declarations":selected,"pagination":pagination});
                json_response(&page)
            }
        }));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let endpoint = listener.local_addr()?;
        let service = tokio::spawn(async move { axum::serve(listener, app).await });
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(
            directory.path(),
            &format!(
                "[global]\nenabled = true\nendpoint = 'http://{endpoint}/rift'\nattempts = 1\n\n\
                 [[dependencies.packages]]\nmanager = 'cargo'\nregistry = 'crates.io'\nname = 'demo'\nversion = '1.0.0'\n"
            ),
        )?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let result = server
            .graph_query(QueryParams {
                query: format!(
                    "SELECT ?i ?text {{ ?syntax_element :of <{ROOT}>; :index ?i; :source ?text }} ORDER BY ?i"
                ),
                language: QueryLanguage::Sparql,
            ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        service.abort();
        let QueryReadResult::Results(result) = result else {
            return Err(format!("expected exact syntax element rows, received {result:?}").into());
        };
        let QueryAnswer::Bindings { bindings, .. } = result.answer else {
            return Err("expected exact syntax element bindings".into());
        };
        assert_eq!(pages.load(std::sync::atomic::Ordering::Relaxed), 1);
        let sources = bindings
            .iter()
            .map(|row| match &row["text"] {
                rift_protocol::query::RdfTerm::Literal { value, .. } => value.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>();
        assert_eq!(sources, ["fn root() {}", "fn root() {}", "root", "root"]);
        Ok(())
    }

    #[tokio::test]
    async fn graph_query_selects_the_local_graph_and_its_complete_symbol_inventory() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='graph_fixture'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
        )?;
        std::fs::write(
            directory.path().join("lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\n",
        )?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let params = QueryParams {
            query: "SELECT (COUNT(?symbol) AS ?n) FROM NAMED <urn:rift:graph:local> { GRAPH <urn:rift:graph:local> { ?symbol a :Symbol ; :name ?name FILTER(?name IN ('alpha', 'beta')) } }".to_owned(),
            language: QueryLanguage::Sparql,
        ..Default::default()
        };
        let result = server
            .graph_query(params)
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        assert!(
            matches!(result, QueryReadResult::Results(ref result)
            if matches!(&result.answer, QueryAnswer::Bindings { bindings, .. }
                if matches!(&bindings[0]["n"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "2"))),
            "{result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn graph_query_projects_regex_occurrences_from_the_original_producer() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(
            directory.path().join("lib.rs"),
            "fn a() { /* signal signal */ }\n",
        )?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let params = QueryParams {
            query: "SELECT ?start ?path { SERVICE :search { ?hit :pattern 'signal'; :target 'file'; :path ?path; :start ?start; :matched_by/:value 'content' } } ORDER BY ?start".to_owned(),
            language: QueryLanguage::Sparql,
        ..Default::default()
        };
        let result = server
            .graph_query(params)
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        let QueryReadResult::Results(result) = result else {
            return Err(format!("expected producer regex results, received {result:?}").into());
        };
        let QueryAnswer::Bindings { bindings, .. } = result.answer else {
            return Err("expected regex bindings".into());
        };
        assert_eq!(bindings.len(), 2, "{bindings:?}");
        let positions = bindings
            .iter()
            .filter_map(|binding| match binding.get("start") {
                Some(rift_protocol::query::RdfTerm::Literal { value, .. }) => Some(value.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(positions, ["12", "19"]);
        Ok(())
    }

    /// The regex occurrences of `signal` one statement answers, or the unavailability it
    /// answered with.
    async fn signal_occurrences(
        server: &RiftMcp,
        scope_input: &str,
    ) -> Result<Result<usize, SearchUnavailable>, Box<dyn std::error::Error + Send + Sync>> {
        let params = QueryParams {
            query: format!(
                "SELECT ?start {{ SERVICE :search {{ ?hit :pattern 'signal'; {scope_input} :target 'file'; :start ?start }} }}"
            ),
            language: QueryLanguage::Sparql,
            ..Default::default()
        };
        let result = server
            .graph_query(params)
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        match result {
            QueryReadResult::Unavailable(unavailable) => Ok(Err(unavailable)),
            QueryReadResult::Results(result) => match result.answer {
                QueryAnswer::Bindings { bindings, .. } => Ok(Ok(bindings.len())),
                other => Err(format!("expected bindings, received {other:?}").into()),
            },
        }
    }

    /// `[search] default_scope = "global"` sends a service naming no `:scope` to package
    /// facts, which this workspace disables, while a service naming `local` still reads
    /// the project.
    #[tokio::test]
    async fn graph_query_reads_the_configured_default_scope_for_a_service_naming_none() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(
            directory.path(),
            "[global]\nenabled = false\n\n[search]\ndefault_scope = \"global\"\n",
        )?;
        std::fs::write(
            directory.path().join("lib.rs"),
            "fn a() { /* signal signal */ }\n",
        )?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;

        let unnamed = signal_occurrences(&server, "").await?;
        let global = signal_occurrences(&server, ":scope 'global';").await?;
        let local = signal_occurrences(&server, ":scope 'local';").await?;

        assert_eq!(local, Ok(2), "a named local scope reads the project");
        assert_eq!(unnamed, global, "an unnamed scope reads the default");
        let Err(SearchUnavailable::Unavailable { warnings, .. }) = unnamed else {
            return Err(format!("expected disabled package facts, received {unnamed:?}").into());
        };
        assert!(
            warnings.contains(&ReadWarning::GlobalAccessDisabled),
            "{warnings:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn graph_ask_refuses_uncovered_edges() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(directory.path().join("lib.rs"), "fn alpha() {}\n")?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        for query in [
            "ASK { ?a :calls ?b }",
            "SELECT (COUNT(?usage) AS ?n) FROM <urn:rift:graph:local> { ?symbol :usages ?usage }",
        ] {
            let result = server
                .graph_query(QueryParams {
                    query: query.to_owned(),
                    language: QueryLanguage::Sparql,
                    ..Default::default()
                })
                .await
                .map_err(|failure| format!("{failure:?}"))?
                .0
                .result;
            assert!(
                matches!(
                    result,
                    QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
                        reason: SymbolUnavailableReason::InsufficientCoverage,
                        ..
                    })
                ),
                "{query}: {result:?}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn graph_ask_accepts_static_call_witness_without_a_language_engine() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(
            directory.path().join("main.mjs"),
            "import { start as run } from './helper.mjs';\n\
             export function caller() { run(); { const run = 0; run(); } }\n",
        )?;
        std::fs::write(
            directory.path().join("helper.mjs"),
            "export function start() {}\nexport function decoy() {}\n",
        )?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let source_symbol = static_endpoint(&server, "caller").await?;
        let target_symbol = static_endpoint(&server, "start").await?;
        for query in [
            format!("ASK {{ <{source_symbol}> :calls <{target_symbol}> }}"),
            format!("ASK {{ <{source_symbol}> :calls ?callee . ?callee :name 'start' }}"),
            format!("ASK {{ ?caller :calls <{target_symbol}> . ?caller :name 'caller' }}"),
            format!("ASK {{ <{target_symbol}> :usages ?usage . ?usage :source 'run' }}"),
        ] {
            let result = server
                .graph_query(QueryParams {
                    query: query.clone(),
                    language: QueryLanguage::Sparql,
                    ..Default::default()
                })
                .await
                .map_err(|failure| format!("{failure:?}"))?
                .0
                .result;
            assert!(
                matches!(&result, QueryReadResult::Results(result)
                    if matches!(&result.answer, QueryAnswer::Boolean { value: true })),
                "{query}: {result:?}"
            );
        }
        for query in [
            format!("ASK {{ <{source_symbol}> :calls ?callee . ?callee :name 'decoy' }}"),
            format!(
                "ASK {{ <{source_symbol}> :name 'caller' . FILTER NOT EXISTS {{ <{source_symbol}> :calls ?callee . ?callee :name 'decoy' }} }}"
            ),
        ] {
            let result = server
                .graph_query(QueryParams {
                    query: query.clone(),
                    language: QueryLanguage::Sparql,
                    ..Default::default()
                })
                .await
                .map_err(|failure| format!("{failure:?}"))?
                .0
                .result;
            assert!(
                matches!(
                    result,
                    QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
                        reason: SymbolUnavailableReason::InsufficientCoverage,
                        ..
                    })
                ),
                "{query}: {result:?}"
            );
        }
        Ok(())
    }

    /// Asserts that whole-inventory and seeded reads of one workspace answer static edges to
    /// `expected` dependency and standard-library symbols, and that the inventory names
    /// the edge kinds whose sites static analysis left without a target.
    async fn assert_external_edges(
        files: &[(&str, &str)],
        heritage: &str,
        expected: &[&str],
    ) -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        for (path, text) in files {
            let path = directory.path().join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, text)?;
        }
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let caller = static_endpoint(&server, "encode").await?;
        let mut answers = String::new();
        for query in [
            "SELECT ?a ?b { ?a :calls ?b }".to_owned(),
            format!("SELECT ?a ?b {{ ?a {heritage} ?b }}"),
        ] {
            let output = server
                .graph_query(QueryParams {
                    query: query.clone(),
                    language: QueryLanguage::Sparql,
                    max_results: 100,
                    ..Default::default()
                })
                .await
                .map_err(|failure| format!("{failure:?}"))?
                .0
                .result;
            assert!(
                matches!(&output, QueryReadResult::Results(_)),
                "{query}: {output:?}"
            );
            let rendered = format!("{output:?}");
            assert!(
                rendered.contains("EdgeCoverageMissing") && rendered.contains("Calls"),
                "{query}: {rendered}"
            );
            answers.push_str(&rendered);
        }
        for target in expected {
            assert!(answers.contains(target), "{target} not in {answers}");
        }
        let target = expected.first().ok_or("an expected target")?;
        let seeded = server
            .graph_query(QueryParams {
                query: format!("ASK {{ <{caller}> :calls <{target}> }}"),
                language: QueryLanguage::Sparql,
                ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        assert!(
            matches!(&seeded, QueryReadResult::Results(result)
                if matches!(&result.answer, QueryAnswer::Boolean { value: true })),
            "{caller}: {seeded:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn graph_query_serves_rust_edges_to_dependency_and_standard_library_symbols() -> TestResult
    {
        let files = [
            (
                "Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nserde_json = \"1\"\n",
            ),
            (
                "src/lib.rs",
                "use std::fmt;\npub struct Task;\nimpl fmt::Display for Task {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { Ok(()) }\n}\npub fn encode(task: &Task) -> String { task.label(); serde_json::to_string(&1).unwrap_or_default() }\n",
            ),
        ];
        assert_external_edges(
            &files,
            ":implements",
            &[
                "rift://symbol/cargo/crates.io/serde_json/rust/serde_json/to_string",
                "rift://symbol/stdlib/rust/rust/std/fmt/Display",
            ],
        )
        .await
    }

    #[tokio::test]
    async fn graph_query_serves_python_edges_to_dependency_and_standard_library_symbols()
    -> TestResult {
        let files = [
            (
                "pyproject.toml",
                "[project]\nname = 'app'\nversion = '0.1.0'\ndependencies = ['greeting==1.0.0']\n[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['app']\n",
            ),
            (
                "app.py",
                "import os\nfrom greeting.core import greet, Base\n\nclass Child(Base):\n    pass\n\ndef encode(items):\n    greet()\n    items.sort()\n    return len(os.path.join('a', 'b'))\n",
            ),
        ];
        assert_external_edges(
            &files,
            ":extends",
            &[
                "rift://symbol/pypi/pypi.org/greeting/python/greeting/core/greet",
                "rift://symbol/stdlib/cpython/python/builtins/len",
                "rift://symbol/pypi/pypi.org/greeting/python/greeting/core/Base",
            ],
        )
        .await
    }

    #[tokio::test]
    async fn graph_query_serves_typescript_edges_to_dependency_and_standard_library_symbols()
    -> TestResult {
        let files = [
            (
                "package.json",
                "{\"name\":\"app\",\"version\":\"1.0.0\",\"dependencies\":{\"zod\":\"4.6.7\"}}",
            ),
            (
                "src/app.ts",
                "import { parse, ZodType } from \"zod\";\nexport class Shape implements ZodType {}\nexport function encode() { undeclared(); return parse(1); }\n",
            ),
        ];
        assert_external_edges(
            &files,
            ":implements",
            &[
                "rift://symbol/npm/npmjs.org/zod/typescript/zod/parse",
                "rift://symbol/npm/npmjs.org/zod/typescript/zod/ZodType",
            ],
        )
        .await
    }

    pub(super) async fn static_endpoint(
        server: &RiftMcp,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let name_literal = oxrdf::Literal::from(name);
        let result = server
            .graph_query(QueryParams {
                query: format!("SELECT ?symbol {{ ?symbol a :Symbol; :name {name_literal} }}"),
                language: QueryLanguage::Sparql,
                ..Default::default()
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
            .result;
        let QueryReadResult::Results(result) = result else {
            return Err(format!("expected a captured symbol for {name}, found {result:?}").into());
        };
        let QueryAnswer::Bindings { bindings, .. } = result.answer else {
            return Err(format!("expected captured symbol bindings for {name}").into());
        };
        match bindings.as_slice() {
            [row] => match row.get("symbol") {
                Some(rift_protocol::query::RdfTerm::Iri { value }) => Ok(value.clone()),
                other => Err(format!("expected an IRI for {name}, found {other:?}").into()),
            },
            _ => Err(format!("expected one symbol for {name}, found {bindings:?}").into()),
        }
    }
}
