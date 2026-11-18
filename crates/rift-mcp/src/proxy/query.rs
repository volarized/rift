//! Routes graph queries and combines the existing project retrieval producers.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use rift_error::{RiftError, errors};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::query::{QueryAnswer, QueryParams, QueryReadResult, RdfTerm};
use rift_protocol::read::{SearchParams, SearchParamsTarget, SearchResult, SearchScope, SymbolId};
use rift_protocol::search_read::{SearchReadResult, SearchUnavailable};
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use rift_search::sparql::{GraphDataset, GraphLimits, GraphQuery, QueryCancellation};
use rmcp::handler::server::tool::IntoCallToolResult as _;
use rmcp::model::{CallToolRequestParams, CallToolResponse};

use super::{RiftProxy, identity};
use crate::output::{QueryDisplay, QueryOutput};
use crate::server::candidates::{QueryPageOptions, collect_retrieval, query_page};
use crate::validation::ConfigurationState;

const QUERY_WORKERS: usize = 2;
const VIEW_PREFIX: &str = "proxy.";

type Response = Result<CallToolResponse, rmcp::ErrorData>;

#[derive(Debug)]
pub(super) struct QueryRoutes {
    blocking: Arc<tokio::sync::Semaphore>,
}

impl Default for QueryRoutes {
    fn default() -> Self {
        Self {
            blocking: Arc::new(tokio::sync::Semaphore::new(QUERY_WORKERS)),
        }
    }
}

impl QueryRoutes {
    async fn work<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, RiftError> + Send + 'static,
    ) -> Result<T, RiftError> {
        let permit = Arc::clone(&self.blocking)
            .acquire_owned()
            .await
            .map_err(|_| query_failure("the query worker is closed"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(worker_failure)?
    }
}

#[derive(Debug)]
enum ProducerFailure {
    Internal(RiftError),
    Forwarded(Box<Response>),
}

impl From<RiftError> for ProducerFailure {
    fn from(error: RiftError) -> Self {
        Self::Internal(error)
    }
}

impl RiftProxy {
    /// Uses the primary backend for ordinary queries. Explicit registered owners route
    /// to their project. The proxy evaluates retrieval spanning accepted projects.
    pub(super) async fn query_forward(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Response, RiftError> {
        let parameters: QueryParams =
            match serde_json::from_value(request.arguments.clone().unwrap_or_default().into()) {
                Ok(parameters) => parameters,
                Err(_) => return self.forward_read(request.clone()).await,
            };
        rift_server::accept_page(parameters.page)?;
        rift_server::accepted_max_results(
            parameters.max_results,
            rift_protocol::query::QUERY_MAX_RESULTS,
        )?;
        let limits = GraphLimits::default();
        let deadline = tokio::time::Instant::now() + self.forward_budget;
        let root = Arc::clone(&self.root);
        let parsed = tokio::time::timeout_at(
            deadline,
            self.query_routes
                .work(move || parsed_query(&root, &parameters.query, &limits)),
        )
        .await
        .map_err(|_| query_failure("query parsing exceeded its request deadline"))?;
        let Ok(query) = parsed else {
            return self.forward_read(request.clone()).await;
        };
        let mut response = self.query_plan(request, query, limits, deadline).await?;
        if let Ok(CallToolResponse::Complete(result)) = &mut response {
            QueryOutput::take_display(&mut result.meta);
        }
        Ok(response)
    }

    async fn query_plan(
        &self,
        request: &CallToolRequestParams,
        query: GraphQuery,
        limits: GraphLimits,
        deadline: tokio::time::Instant,
    ) -> Result<Response, RiftError> {
        if let Some(adjacency) = query.adjacency_selection() {
            let owner = SymbolIdentity::parse(adjacency.seed.as_str())?
                .owner()
                .clone();
            if let Some(backend) = identity::backend_key(&owner) {
                let local = identity::dispatch_id(&adjacency.seed, &backend)?;
                let mut dispatched = request.clone();
                let arguments = dispatched.arguments.get_or_insert_default();
                arguments.insert(
                    "query".to_owned(),
                    query.dispatched_adjacency(&local)?.into(),
                );
                return self.forward_query_to(&backend, dispatched).await;
            }
        }
        let mut combined = false;
        for service in query.retrieval_requests() {
            let backends = self.candidate_backends(&service.request)?;
            combined |= backends.len() != 1 || backends[0].0 != "local";
        }
        if !combined {
            return self.forward_read(request.clone()).await;
        }
        if query.reads_base()
            || query
                .retrieval_requests()
                .iter()
                .any(|service| service.flow_limit.is_some())
        {
            return Ok(render(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
        }
        tokio::time::timeout_at(deadline, self.evaluate_retrieval(request, query, limits))
            .await
            .map_err(|_| query_failure("combined retrieval exceeded its request deadline"))?
    }

    async fn forward_query_to(
        &self,
        backend: &str,
        request: CallToolRequestParams,
    ) -> Result<Response, RiftError> {
        let owner = self
            .backends
            .get(backend)
            .ok_or_else(|| self.extra.refusal(backend, "search", "scope"))?;
        let response = owner.forward_read(request).await?;
        let Ok(CallToolResponse::Complete(result)) = &response else {
            return Ok(response);
        };
        if result.is_error == Some(true) {
            return Ok(response);
        }
        let mut answer: QueryReadResult = decode(result)?;
        match &mut answer {
            QueryReadResult::Results(result) => {
                qualify_answer(&mut result.answer, backend)?;
                identity::qualify_warnings(&mut result.warnings, backend)?;
            }
            QueryReadResult::Unavailable(SearchUnavailable::Unavailable { warnings, .. }) => {
                identity::qualify_warnings(warnings, backend)?;
            }
        }
        let mut meta = result.meta.clone();
        let display = QueryOutput::take_display(&mut meta);
        let mut qualified = render(QueryOutput::new(answer, display));
        if let Ok(CallToolResponse::Complete(qualified)) = &mut qualified {
            qualified.meta = meta;
        }
        Ok(qualified)
    }

    async fn evaluate_retrieval(
        &self,
        request: &CallToolRequestParams,
        query: GraphQuery,
        limits: GraphLimits,
    ) -> Result<Response, RiftError> {
        let params: QueryParams =
            serde_json::from_value(request.arguments.clone().unwrap_or_default().into())
                .map_err(|error| query_failure(&error.to_string()))?;
        rift_server::accept_page(params.page)?;
        rift_server::accepted_max_results(
            params.max_results,
            rift_protocol::query::QUERY_MAX_RESULTS,
        )?;
        let mut selected = Vec::new();
        let mut bytes_left = limits.literal_bytes_max();
        for service in query.retrieval_requests() {
            let request = &service.request;
            let producer = if request.pattern.is_some() && request.scope == SearchScope::All {
                self.combined_pattern(request, limits).await
            } else {
                collect_retrieval(request.clone(), limits, ProducerFailure::from, |request| {
                    self.query_producer(request)
                })
                .await
            };
            let result = match producer {
                Ok(Ok(result)) => result,
                Ok(Err(reason)) => return Ok(render(QueryReadResult::Unavailable(reason))),
                Err(ProducerFailure::Forwarded(response)) => return Ok(*response),
                Err(ProducerFailure::Internal(error)) => return Err(error),
            };
            let bytes = super::encode_bounded(&result, bytes_left)
                .map_err(|_| query_failure("combined retrieval exceeds its byte bound"))?;
            bytes_left -= bytes.len();
            selected.push(result);
        }
        let cancellation = QueryCancellation::new();
        let _guard = CancelQuery(cancellation.clone());
        let root = Arc::clone(&self.root);
        let answer = self
            .query_routes
            .work(move || {
                let configuration = ConfigurationState::accept(&root)
                    .search_configuration()
                    .query;
                let mut dataset = GraphDataset::builder(limits);
                let mut warnings = Vec::new();
                for (service, result) in query.retrieval_requests().iter().zip(&selected) {
                    dataset.insert_retrieval(service, result)?;
                    for warning in &result.warnings {
                        if !warnings.contains(warning) {
                            warnings.push(warning.clone());
                        }
                    }
                }
                let answer = dataset.build().evaluate(&query, &cancellation)?.into();
                let view = proxy_view(selected.iter().filter_map(|result| result.view.as_ref()))?;
                if cancellation.is_cancelled() {
                    return errors::search::query_cancelled().fail();
                }
                query_page(
                    &view,
                    &answer,
                    QueryPageOptions {
                        configuration: &configuration,
                        page: params.page,
                        max_results: params.max_results,
                        warnings,
                        display: QueryDisplay::selected(&answer, query.presentation()),
                    },
                )
            })
            .await?;
        Ok(render(answer))
    }

    async fn combined_pattern(
        &self,
        request: &SearchParams,
        limits: GraphLimits,
    ) -> Result<Result<Box<SearchResult>, SearchUnavailable>, ProducerFailure> {
        if request.view.is_some() {
            return Ok(Err(unavailable_read(SymbolUnavailableReason::ViewContext)));
        }
        let mut results = Vec::new();
        let mut warnings = Vec::new();
        let mut views = Vec::new();
        let mut bytes_left = limits.literal_bytes_max();
        for (name, backend) in self.candidate_backends(request)? {
            let mut dispatched = request.clone();
            dispatched.max_results = None;
            dispatched.page = 1;
            if name != "local" {
                dispatched.scope = SearchScope::Local;
                dispatched.packages.clear();
            }
            let collected = collect_retrieval(dispatched, limits, ProducerFailure::from, |page| {
                backend.direct_producer(page, name)
            })
            .await?;
            let mut result = match collected {
                Ok(result) => result,
                Err(reason) => return Ok(Err(reason)),
            };
            let bytes = super::encode_bounded(&result, bytes_left)
                .map_err(|_| query_failure("combined patterns exceed their byte bound"))?;
            bytes_left -= bytes.len();
            if result.results.len().saturating_add(results.len()) > limits.solutions_max() {
                return Ok(Err(unavailable_read(
                    SymbolUnavailableReason::InsufficientCoverage,
                )));
            }
            results.append(&mut result.results);
            for warning in result.warnings {
                if !warnings.contains(&warning) {
                    warnings.push(warning);
                }
            }
            views.extend(result.view);
        }
        crate::global::order_search_hits(&mut results, request.order);
        let limit = if request.max_results.is_some() || request.page != 1 {
            rift_server::search_page_limit(request)?
        } else {
            results.len().max(1)
        };
        let (results, pagination) = crate::global::page_window(results, request.page, limit);
        Ok(Ok(Box::new(SearchResult {
            view: Some(proxy_view(views.iter())?),
            revision: None,
            results,
            pagination,
            warnings,
        })))
    }

    async fn query_producer(
        &self,
        request: SearchParams,
    ) -> Result<SearchReadResult, ProducerFailure> {
        let ranking = request.pattern.is_none()
            && request.change.is_none()
            && request.revision.is_none()
            && request.target != SearchParamsTarget::Commit;
        let response = if ranking {
            let request = producer_request("search", &request)?;
            self.captured_search(&request, true)
                .await?
                .ok_or_else(|| query_failure("the ranked producer has no response"))?
        } else {
            let selected = self.candidate_backends(&request)?;
            let [(name, backend)] = selected.as_slice() else {
                return self.direct_producer(request, "local").await;
            };
            let mut dispatched = request;
            if *name != "local" {
                dispatched.scope = SearchScope::Local;
            }
            return backend.direct_producer(dispatched, name).await;
        };
        producer_result(response)
    }
    async fn direct_producer(
        &self,
        request: SearchParams,
        name: &str,
    ) -> Result<SearchReadResult, ProducerFailure> {
        let response = self
            .forward_read(producer_request("search_retrieval", &request)?)
            .await?;
        let mut result = producer_result(response)?;
        identity::qualify_search(&mut result, name)?;
        Ok(result)
    }
}

/// Parses one statement and selects the workspace's `[search] default_scope` for every
/// service naming no `:scope`, so backend routing meets the scope the primary server
/// evaluates the same statement under.
///
/// Each call accepts `rift.toml` again, as the server does after an edit: two reads of a
/// file of at most `CONFIGURATION_FILE_BYTES_MAX` bytes and one parse. A file acceptance
/// refuses selects `local`, and the primary server then answers with that refusal.
fn parsed_query(
    root: &Path,
    statement: &str,
    limits: &GraphLimits,
) -> Result<GraphQuery, RiftError> {
    let query = GraphQuery::parse(statement, limits)?;
    let default_scope = ConfigurationState::accept(root)
        .search_configuration()
        .default_scope;
    Ok(query.with_default_scope(&default_scope))
}

fn producer_request(
    name: &'static str,
    request: &SearchParams,
) -> Result<CallToolRequestParams, RiftError> {
    let arguments = serde_json::to_value(request).map_err(encoding_failure)?;
    let serde_json::Value::Object(arguments) = arguments else {
        return Err(query_failure("producer request is not an object"));
    };
    Ok(CallToolRequestParams::new(name).with_arguments(arguments))
}

fn producer_result(response: Response) -> Result<SearchReadResult, ProducerFailure> {
    if let Ok(CallToolResponse::Complete(result)) = &response
        && result.is_error != Some(true)
    {
        return Ok(decode(result)?);
    }
    Err(ProducerFailure::Forwarded(Box::new(response)))
}

fn decode<T: serde::de::DeserializeOwned>(
    result: &rmcp::model::CallToolResult,
) -> Result<T, RiftError> {
    let value = result
        .structured_content
        .as_ref()
        .ok_or_else(|| query_failure("the backend has no structured query result"))?;
    super::encode_bounded(value, crate::global::candidates::CANDIDATE_BYTES_MAX)
        .map_err(encoding_failure)?;
    serde_json::from_value(value.clone()).map_err(encoding_failure)
}

fn proxy_view<'a>(
    mut views: impl Iterator<Item = &'a CapturedView>,
) -> Result<CapturedView, RiftError> {
    let expires = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_mins(15))
        .map_err(|_| query_failure("query expiry is out of range"))?;
    let expires = views.try_fold(expires, |expires, view| {
        view.expires_at
            .parse::<jiff::Timestamp>()
            .map(|selected| selected.min(expires))
            .map_err(|_| query_failure("invalid producer expiry"))
    })?;
    let id = CapturedViewId::parse(&format!("{VIEW_PREFIX}{}", crate::http::mint_token()?))?;
    Ok(CapturedView {
        id,
        expires_at: expires.to_string(),
    })
}

fn qualify_answer(answer: &mut QueryAnswer, backend: &str) -> Result<(), RiftError> {
    match answer {
        QueryAnswer::Bindings { bindings, .. } => {
            for term in bindings.iter_mut().flat_map(BTreeMap::values_mut) {
                qualify_term(term, backend)?;
            }
        }
        QueryAnswer::Graph { triples } => {
            for triple in triples {
                for term in [
                    &mut triple.subject,
                    &mut triple.predicate,
                    &mut triple.object,
                ] {
                    qualify_term(term, backend)?;
                }
            }
        }
        QueryAnswer::Boolean { .. } => {}
    }
    Ok(())
}

fn qualify_term(term: &mut RdfTerm, backend: &str) -> Result<(), RiftError> {
    if let RdfTerm::Iri { value } = term
        && value.starts_with("rift://symbol/local/")
        && let Ok(mut id) = SymbolId::parse(value)
    {
        identity::qualify_id(&mut id, backend)?;
        value.clear();
        value.push_str(id.as_str());
    }
    Ok(())
}

fn unavailable(reason: SymbolUnavailableReason) -> QueryReadResult {
    QueryReadResult::Unavailable(unavailable_read(reason))
}

fn unavailable_read(reason: SymbolUnavailableReason) -> SearchUnavailable {
    SearchUnavailable::Unavailable {
        reason,
        view: None,
        warnings: Vec::new(),
    }
}

fn render(answer: impl Into<QueryOutput>) -> Response {
    crate::output::Json(answer.into()).into_call_tool_result()
}

fn query_failure(detail: &str) -> RiftError {
    errors::server::read_unavailable()
        .operation("search")
        .detail(detail)
        .error()
}

fn worker_failure(error: tokio::task::JoinError) -> RiftError {
    errors::mcp::forward_failed().source(error).error()
}

fn encoding_failure(error: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(error).error()
}

struct CancelQuery(QueryCancellation);
impl Drop for CancelQuery {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
mod tests;
