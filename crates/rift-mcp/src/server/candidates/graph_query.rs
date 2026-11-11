//! Graph query execution and retention under the captured workspace owners.

use std::sync::Arc;
use std::time::SystemTime;

use rift_error::{RiftError, errors};
use rift_protocol::query::{
    QueryAnswer, QueryConfiguration, QueryParams, QueryReadResult, QueryResult,
};
use rift_protocol::read::{Pagination, ReadWarning, SearchResult};
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use rift_search::sparql::{
    AdjacencySelection, GraphDataset, GraphLimits, GraphQuery, PageForm, PageSelection,
    QueryCancellation, continuation_query,
};
use serde::{Deserialize, Serialize};

use super::{BYTES_MAX, CandidateResponses, RiftMcp, read_failure};
use crate::output::{Json, ToolFailure};

mod exact;
mod installed;
mod relationships;
pub(crate) mod retrieval;

use super::super::RequestDeadline;
use relationships::AcquiredAdjacency;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedQuery {
    root: std::path::PathBuf,
    owner: rift_protocol::identity::SymbolOwner,
    view: CapturedView,
    template_end: usize,
    pages: Vec<QueryPage>,
    warnings: Vec<ReadWarning>,
}

#[derive(Deserialize, Serialize)]
struct QueryPage {
    start: usize,
    end: usize,
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
    ) -> Result<Json<QueryReadResult>, ToolFailure> {
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
        if let Some(selection) = query.page_selection() {
            return self.replay_graph_query(selection).await.map(Json);
        }
        let graph = match Box::pin(self.prepare_graph(&query, limits, deadline)).await? {
            Ok(graph) => graph,
            Err(unavailable) => return Ok(Json(QueryReadResult::Unavailable(unavailable))),
        };
        let views = Arc::clone(&self.local_views);
        let responses = Arc::clone(&self.candidate_responses);
        let cancellation = QueryCancellation::new();
        let _guard = QueryGuard(cancellation.clone());
        tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run("graph query", move || {
                graph.evaluate(&query, limits, deadline, &cancellation, &views, &responses)
            }),
        )
        .await
        .map_err(|_| read_failure(deadline_failure(deadline.budget())))?
        .map(Json)
        .map_err(read_failure)
    }

    async fn replay_graph_query(
        &self,
        selection: PageSelection,
    ) -> Result<QueryReadResult, ToolFailure> {
        let responses = Arc::clone(&self.candidate_responses);
        let views = Arc::clone(&self.local_views);
        self.blocking
            .run("retained graph query", move || {
                let (saved, answer) = match responses.replay_query(&selection)? {
                    Ok(result) => result,
                    Err(reason) => return Ok(query_unavailable(reason)),
                };
                let views = views.get().ok_or_else(retention_failure)?;
                if let Err(reason) =
                    views.captured_view(selection.view.as_str(), &saved.root, &saved.owner)?
                {
                    return Ok(query_unavailable(reason));
                }
                saved.response(answer, selection.page)
            })
            .await
            .map_err(read_failure)
    }

    async fn prepare_graph(
        &self,
        query: &GraphQuery,
        limits: GraphLimits,
        deadline: RequestDeadline,
    ) -> Result<Result<AcquiredGraph, SearchUnavailable>, ToolFailure> {
        let resolved = self
            .published_workspace(super::super::wire::ErrorPhase::Read, deadline)
            .await?;
        resolved
            .published
            .configuration
            .accepted_for_tool(super::super::wire::ErrorPhase::Read)?;
        if resolved.published.preparation.is_some() {
            return Ok(Err(unavailable(SymbolUnavailableReason::IndexPreparing)));
        }
        let selected = query.adjacency_selection();
        let mut acquired = if let Some(selected) = &selected {
            let result = tokio::time::timeout_at(
                deadline.at(),
                Box::pin(self.acquire_adjacency(&resolved, selected, limits, deadline)),
            )
            .await
            .map_err(|_| read_failure(deadline_failure(deadline.budget())))?;
            match result {
                Ok(Ok(acquired)) => acquired,
                Ok(Err(unavailable)) => return Ok(Err(unavailable)),
                Err(error) if lacks_coverage(&error) => {
                    return Ok(Err(unavailable(
                        SymbolUnavailableReason::InsufficientCoverage,
                    )));
                }
                Err(error) => return Err(read_failure(error)),
            }
        } else if query.symbol_selection().is_some()
            || query.node_selection().is_some()
            || !query.reads_base()
        {
            AcquiredAdjacency::default()
        } else {
            return Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
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
        let mut retrieved = Vec::new();
        for selection in query.retrieval_requests() {
            let result = tokio::time::timeout_at(
                deadline.at(),
                Box::pin(self.query_retrieval(selection, limits)),
            )
            .await
            .map_err(|_| read_failure(deadline_failure(deadline.budget())))??;
            match result {
                Ok(result) => retrieved.push(*result),
                Err(reason) => return Ok(Err(reason)),
            }
        }
        if !retrieved.is_empty() && !self.engine_tree_matches(&resolved.published).await? {
            return Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
        }
        let configuration = resolved
            .published
            .configuration
            .search_configuration()
            .query;
        let reads = Arc::clone(&resolved.published.reads);
        let mut warnings = acquired.take_warnings();
        warnings.extend(resolved.stale);
        if let Some(exact) = &mut exact {
            warnings.extend(exact.take_warnings());
        }
        for warning in retrieved.iter().flat_map(|result| &result.warnings) {
            if !warnings.contains(warning) {
                warnings.push(warning.clone());
            }
        }
        Ok(Ok(AcquiredGraph {
            reads,
            selected,
            acquired,
            exact,
            retrieved,
            configuration,
            warnings,
        }))
    }
}

struct AcquiredGraph {
    reads: Arc<rift_server::ReadService>,
    selected: Option<AdjacencySelection>,
    acquired: AcquiredAdjacency,
    exact: Option<exact::ExactFacts>,
    retrieved: Vec<SearchResult>,
    configuration: QueryConfiguration,
    warnings: Vec<ReadWarning>,
}

fn project_local_endpoints(
    input: &rift_server::LocalSymbolViewInput,
    reads: &rift_server::ReadService,
    acquired: &relationships::AcquiredAdjacency,
    selected: &rift_search::sparql::AdjacencySelection,
    dataset: &mut rift_search::sparql::GraphDatasetBuilder,
    cancellation: &QueryCancellation,
    deadline: std::time::Instant,
) -> Result<(), RiftError> {
    if !selected.symbol_details && selected.node_source.is_none() {
        return Ok(());
    }
    for endpoint in acquired.local_endpoints() {
        let identity = rift_protocol::identity::SymbolIdentity::parse(endpoint.as_str())?;
        if identity.owner().is_local() {
            let symbols = rift_search::sparql::SymbolSelection::Identified(endpoint.as_str());
            if let Some(source) = selected.node_source {
                input.project_nodes(
                    reads,
                    rift_search::sparql::NodeSelection { symbols, source },
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

impl AcquiredGraph {
    fn evaluate(
        self,
        query: &GraphQuery,
        limits: GraphLimits,
        deadline: RequestDeadline,
        cancellation: &QueryCancellation,
        views: &std::sync::OnceLock<rift_server::LocalSymbolViews>,
        responses: &CandidateResponses,
    ) -> Result<QueryReadResult, RiftError> {
        let Self {
            reads,
            selected,
            acquired,
            exact,
            retrieved,
            configuration,
            warnings,
        } = self;
        let input = reads.local_search_view_input(true)?;
        let mut dataset = GraphDataset::builder(limits);
        if let Some(selected) = &selected {
            if let Err(error) = acquired.project(&mut dataset, selected, &input) {
                if lacks_coverage(&error) {
                    return Ok(query_unavailable(
                        SymbolUnavailableReason::InsufficientCoverage,
                    ));
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
        } else if let Some(exact) = &exact {
            exact.project(&mut dataset)?;
        } else if let Some(selection) = query.node_selection() {
            input.project_nodes(
                &reads,
                selection,
                &mut dataset,
                cancellation,
                deadline.at().into_std(),
            )?;
        } else if let Some(selection) = query.symbol_selection() {
            input.project_symbols(
                selection,
                &mut dataset,
                cancellation,
                deadline.at().into_std(),
            )?;
        }
        for (selection, result) in query.retrieval_requests().iter().zip(&retrieved) {
            dataset.insert_retrieval(selection, result)?;
        }
        if query.names_local_graph() {
            if selected.is_some() {
                return Ok(query_unavailable(
                    SymbolUnavailableReason::InsufficientCoverage,
                ));
            }
            dataset.publish_local_graph()?;
        }
        let dataset = dataset.build();
        let answer: QueryAnswer = dataset.evaluate(query, cancellation)?.into();
        let views = views.get().ok_or_else(retention_failure)?;
        let root = input.root().to_owned();
        let owner = input.owner().clone();
        let mut response = None;
        views.capture_with(input, |view| {
            let mut view = view.clone();
            let provider_views = acquired
                .remote_view()
                .into_iter()
                .chain(retrieved.iter().filter_map(|result| result.view.as_ref()));
            for provider in provider_views {
                let expiry = crate::proxy::captured_expiry(provider)?;
                if expiry <= SystemTime::now() {
                    response = Some(query_unavailable(SymbolUnavailableReason::ViewExpired));
                    return Ok(false);
                }
                if expiry < crate::proxy::captured_expiry(&view)? {
                    view.expires_at.clone_from(&provider.expires_at);
                }
            }
            if cancellation.is_cancelled() {
                return errors::search::query_cancelled().fail();
            }
            response = Some(responses.store_query(
                &root,
                &owner,
                &view,
                &answer,
                &configuration,
                warnings,
            )?);
            Ok(true)
        })?;
        response.ok_or_else(retention_failure)
    }
}

impl CandidateResponses {
    /// Retains one evaluated query and returns its first transport page.
    pub(crate) fn store_query(
        &self,
        root: &std::path::Path,
        owner: &rift_protocol::identity::SymbolOwner,
        view: &CapturedView,
        answer: &QueryAnswer,
        configuration: &QueryConfiguration,
        warnings: Vec<ReadWarning>,
    ) -> Result<QueryReadResult, RiftError> {
        let (saved, bytes, first) =
            SavedQuery::encoded(root, owner, view, answer, configuration, warnings)?;
        let maximum =
            usize::try_from(configuration.page_limit.bytes()).map_err(|_| page_failure())?;
        if encoded_size(&first)? > maximum {
            return Err(page_failure());
        }
        self.save_query(&saved, &bytes)?;
        Ok(first)
    }

    /// Replays a query owned by the original proxy connection without reacquiring facts.
    pub(crate) fn retained_proxy_query(
        &self,
        selection: &PageSelection,
        root: &std::path::Path,
        owner: &rift_protocol::identity::SymbolOwner,
    ) -> Result<QueryReadResult, RiftError> {
        let (saved, answer) = match self.replay_query(selection)? {
            Ok(found) => found,
            Err(reason) => return Ok(query_unavailable(reason)),
        };
        if saved.root != root || &saved.owner != owner {
            return Ok(query_unavailable(SymbolUnavailableReason::ViewContext));
        }
        saved.response(answer, selection.page)
    }

    fn save_query(&self, saved: &SavedQuery, bytes: &[u8]) -> Result<(), RiftError> {
        let mut encoded =
            crate::proxy::encode_bounded(saved, BYTES_MAX).map_err(encoding_failure)?;
        let additional = bytes.len().checked_add(1).ok_or_else(retention_failure)?;
        if encoded
            .len()
            .checked_add(additional)
            .is_none_or(|len| len > BYTES_MAX)
        {
            return Err(retention_failure());
        }
        encoded
            .try_reserve_exact(additional)
            .map_err(|_| retention_failure())?;
        encoded.push(b'\n');
        encoded.extend_from_slice(bytes);
        let expires = crate::proxy::captured_expiry(&saved.view)?;
        self.save_bytes(saved.view.id.as_str().to_owned(), expires, encoded)
    }

    fn replay_query(
        &self,
        selection: &PageSelection,
    ) -> Result<Result<(SavedQuery, QueryAnswer), SymbolUnavailableReason>, RiftError> {
        let rows = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(row) = rows.iter().find(|row| row.id == selection.view.as_str()) else {
            return Ok(Err(SymbolUnavailableReason::ViewExpired));
        };
        if row.expires <= SystemTime::now() {
            return Ok(Err(SymbolUnavailableReason::ViewExpired));
        }
        let Some(separator) = row.bytes.iter().position(|byte| *byte == b'\n') else {
            return Ok(Err(SymbolUnavailableReason::ViewContext));
        };
        let saved: SavedQuery =
            serde_json::from_slice(&row.bytes[..separator]).map_err(encoding_failure)?;
        let answer = saved.decode_bytes(&row.bytes[separator + 1..], Some(selection.page))?;
        let matches = match (&selection.form, &answer) {
            (
                PageForm::Bindings {
                    variables: selected,
                },
                QueryAnswer::Bindings { variables, .. },
            ) => selected == variables,
            (PageForm::Graph, QueryAnswer::Graph { .. }) => true,
            _ => false,
        };
        if matches {
            Ok(Ok((saved, answer)))
        } else {
            Ok(Err(SymbolUnavailableReason::ViewContext))
        }
    }
}

fn deadline_failure(budget: std::time::Duration) -> RiftError {
    errors::search::query_deadline_exceeded()
        .timeout_ms(u64::try_from(budget.as_millis()).unwrap_or(u64::MAX))
        .error()
}

fn empty_answer(answer: &QueryAnswer) -> QueryAnswer {
    match answer {
        QueryAnswer::Bindings { variables, .. } => QueryAnswer::Bindings {
            variables: variables.clone(),
            bindings: Vec::new(),
        },
        QueryAnswer::Graph { .. } => QueryAnswer::Graph {
            triples: Vec::new(),
        },
        QueryAnswer::Boolean { value } => QueryAnswer::Boolean { value: *value },
    }
}

fn encode_records(
    answer: &QueryAnswer,
    maximum: usize,
) -> Result<(Vec<u8>, usize, Vec<usize>), RiftError> {
    let mut bytes = crate::proxy::encode_bounded(&empty_answer(answer), maximum)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    let template_end = bytes.len();
    let mut offsets = vec![template_end];
    match answer {
        QueryAnswer::Bindings { bindings, .. } => {
            for row in bindings {
                append_record(&mut bytes, row, maximum)?;
                offsets.push(bytes.len());
            }
        }
        QueryAnswer::Graph { triples } => {
            for row in triples {
                append_record(&mut bytes, row, maximum)?;
                offsets.push(bytes.len());
            }
        }
        QueryAnswer::Boolean { .. } => {}
    }
    Ok((bytes, template_end, offsets))
}

fn append_record(
    bytes: &mut Vec<u8>,
    row: &impl Serialize,
    maximum: usize,
) -> Result<(), RiftError> {
    let remaining = maximum
        .checked_sub(bytes.len())
        .ok_or_else(retention_failure)?;
    let encoded = crate::proxy::encode_bounded(row, remaining).map_err(encoding_failure)?;
    if bytes
        .len()
        .checked_add(encoded.len())
        .is_none_or(|size| size > maximum)
    {
        return Err(retention_failure());
    }
    bytes.try_reserve_exact(encoded.len()).map_err(|source| {
        retention_failure().with(rift_error::ErrorContext::new(
            "source",
            rift_error::ErrorValue::source(source),
        ))
    })?;
    bytes.extend_from_slice(&encoded);
    Ok(())
}

fn encoded_size(value: &impl Serialize) -> Result<usize, RiftError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(encoding_failure)
}

fn plan_pages(
    answer: &QueryAnswer,
    offsets: &[usize],
    saved: &SavedQuery,
    configuration: &QueryConfiguration,
) -> Result<Vec<QueryPage>, RiftError> {
    let count = offsets.len().saturating_sub(1);
    if matches!(answer, QueryAnswer::Boolean { .. }) {
        return Ok(vec![QueryPage {
            start: saved.template_end,
            end: saved.template_end,
        }]);
    }
    let mut envelope = QueryResult {
        answer: empty_answer(answer),
        view: Some(saved.view.clone()),
        pagination: Pagination {
            page_index: u64::MAX,
            total_pages: u64::MAX,
        }
        .into(),
        warnings: saved.warnings.clone(),
    };
    envelope.pagination.next_query = Some(continuation_query(answer, &saved.view.id, u64::MAX)?);
    let base = encoded_size(&envelope)?;
    let limit = usize::try_from(configuration.page_limit.bytes()).map_err(|_| page_failure())?;
    let mut pages = Vec::new();
    let mut start = 0;
    let mut bytes = base;
    for index in 0..count {
        let size = offsets[index + 1]
            .checked_sub(offsets[index])
            .and_then(|size| size.checked_add(1))
            .ok_or_else(page_failure)?;
        if base.checked_add(size).is_none_or(|size| size > limit) {
            return Err(page_failure());
        }
        let rows = u64::try_from(index - start).map_err(|_| page_failure())?;
        if rows == configuration.page_size
            || bytes.checked_add(size).is_none_or(|size| size > limit)
        {
            pages.push(QueryPage {
                start: offsets[start],
                end: offsets[index],
            });
            start = index;
            bytes = base;
        }
        bytes = bytes.checked_add(size).ok_or_else(page_failure)?;
    }
    if start < count {
        pages.push(QueryPage {
            start: offsets[start],
            end: offsets[count],
        });
    }
    Ok(pages)
}

fn first_page(answer: &QueryAnswer, page: Option<&QueryPage>, offsets: &[usize]) -> QueryAnswer {
    let count = page.map_or(0, |page| {
        offsets.partition_point(|offset| *offset < page.end)
    });
    match answer {
        QueryAnswer::Bindings {
            variables,
            bindings,
        } => QueryAnswer::Bindings {
            variables: variables.clone(),
            bindings: bindings[..count].to_vec(),
        },
        QueryAnswer::Graph { triples } => QueryAnswer::Graph {
            triples: triples[..count].to_vec(),
        },
        QueryAnswer::Boolean { value } => QueryAnswer::Boolean { value: *value },
    }
}

impl SavedQuery {
    fn encoded(
        root: &std::path::Path,
        owner: &rift_protocol::identity::SymbolOwner,
        view: &CapturedView,
        answer: &QueryAnswer,
        configuration: &QueryConfiguration,
        warnings: Vec<ReadWarning>,
    ) -> Result<(Self, Vec<u8>, QueryReadResult), RiftError> {
        let (bytes, template_end, offsets) = encode_records(answer, BYTES_MAX)?;
        let mut saved = Self {
            root: root.to_owned(),
            owner: owner.clone(),
            view: view.clone(),
            template_end,
            pages: Vec::new(),
            warnings,
        };
        saved.pages = plan_pages(answer, &offsets, &saved, configuration)?;
        let response = saved.response(first_page(answer, saved.pages.first(), &offsets), 0)?;
        Ok((saved, bytes, response))
    }

    fn decode_bytes(&self, bytes: &[u8], page: Option<u64>) -> Result<QueryAnswer, RiftError> {
        let template = bytes
            .get(..self.template_end)
            .ok_or_else(retention_failure)?;
        let mut answer: QueryAnswer = serde_json::from_slice(template).map_err(encoding_failure)?;
        let range = match page {
            Some(page) => usize::try_from(page)
                .ok()
                .and_then(|page| self.pages.get(page))
                .map(|page| page.start..page.end)
                .unwrap_or(self.template_end..self.template_end),
            None => self.template_end..bytes.len(),
        };
        let rows = bytes.get(range).ok_or_else(retention_failure)?;
        match &mut answer {
            QueryAnswer::Bindings { bindings, .. } => *bindings = decode_records(rows)?,
            QueryAnswer::Graph { triples } => *triples = decode_records(rows)?,
            QueryAnswer::Boolean { .. } => {}
        }
        Ok(answer)
    }

    fn response(&self, answer: QueryAnswer, page_index: u64) -> Result<QueryReadResult, RiftError> {
        let total_pages = u64::try_from(self.pages.len()).map_err(|_| page_failure())?;
        let next = page_index.checked_add(1).filter(|next| *next < total_pages);
        let mut pagination = rift_protocol::query::QueryPagination::from(Pagination {
            page_index,
            total_pages,
        });
        pagination.next_query = next
            .map(|next| continuation_query(&answer, &self.view.id, next))
            .transpose()?;
        Ok(QueryReadResult::Results(Box::new(QueryResult {
            answer,
            view: Some(self.view.clone()),
            pagination,
            warnings: self.warnings.clone(),
        })))
    }
}

fn decode_records<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<Vec<T>, RiftError> {
    serde_json::Deserializer::from_slice(bytes)
        .into_iter()
        .collect::<Result<Vec<T>, _>>()
        .map_err(encoding_failure)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::query::QueryLanguage;
    use serde_json::{Value, json};

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    #[tokio::test]
    async fn retained_query_pages_keep_the_original_bindings_after_changes() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "[search.query]\npage_size = 1\n")?;
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='graph_fixture'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
        )?;
        let path = directory.path().join("lib.rs");
        std::fs::write(&path, "pub fn alpha() {}\npub fn beta() {}\n")?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let QueryReadResult::Results(first) = server
            .graph_query(QueryParams {
                query: "SELECT ?name { ?symbol a :Symbol; :name ?name; :kind 'function' } ORDER BY ?name".to_owned(),
                language: QueryLanguage::Sparql,
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
        else {
            return Err("expected a query result".into());
        };
        assert_eq!(first.pagination.page.total_pages, 2, "{first:?}");
        let next_query = first
            .pagination
            .next_query
            .clone()
            .ok_or("expected continuation")?;
        std::fs::write(path, "pub fn replacement() {}\n")?;
        let QueryReadResult::Results(next) = server
            .graph_query(QueryParams {
                query: next_query,
                language: QueryLanguage::Sparql,
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0
        else {
            return Err("expected retained query result".into());
        };
        assert_eq!(first.view, next.view);
        assert_eq!(next.pagination.page.page_index, 1);
        assert_eq!(next.pagination.page.total_pages, 2);
        assert!(next.pagination.next_query.is_none());
        let QueryAnswer::Bindings { bindings, .. } = next.answer else {
            return Err("expected retained bindings".into());
        };
        assert!(
            matches!(&bindings[0]["name"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "beta")
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
                "SELECT ?start ?text {{ <{}> :usages ?usage . ?usage :range/:start ?start ; :source ?text }} ORDER BY ?start",
                id.as_str()
            ),
            language: QueryLanguage::Sparql,
        };
        let result = server
            .graph_query(params)
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
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
                language: QueryLanguage::Sparql,
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
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
                language: QueryLanguage::Sparql,
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
        assert!(
            matches!(metadata, QueryReadResult::Results(ref result)
            if matches!(&result.answer, QueryAnswer::Bindings { bindings, .. }
                if bindings.iter().any(|row| matches!(&row["name"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "caller")))),
            "{metadata:?}"
        );
        let evidence = server.graph_query(QueryParams {
            query: format!("SELECT ?source ?unit {{ ?node :of <{}> ; :index 0 ; :source ?source ; :range [ :unit ?unit ; :start ?start ; :end ?end ] }}", id.as_str()),
            language: QueryLanguage::Sparql,
        }).await.map_err(|failure| format!("{failure:?}"))?.0;
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

    fn remote_graph_routes(
        requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
        endpoint_expiry: String,
    ) -> axum::Router {
        use axum::routing::{get, post};

        axum::Router::new()
            .route("/rift/v1/capabilities", get(|| async {
                json_response(&json!({
                    "supported_package_managers": ["cargo"],
                    "supported_features": ["resolutions", "search", "symbols", "relationships", "nodes"],
                    "publication_format": "rift-package-index-v2",
                    "analyzer_revision": "analyzer-v1",
                    "corpus_revision": "corpus-v1",
                    "required_search_fields": ["name","qualified_name","documentation","signature"],
                    "bounds": {
                        "request_body_bytes_max": 1_048_576, "response_body_bytes_max": 1_048_576,
                        "dependency_entries_max": 100, "query_bytes_max": 4096, "query_terms_max": 32,
                        "query_term_bytes_max": 256, "identifiers_max": 16, "identifier_bytes_max": 4096,
                        "packages_max": 100, "page_limit_min": 1, "page_limit_max": 100,
                        "page_limit_default": 20, "cursor_bytes_max": 4096, "candidate_pool_max": 100,
                        "warnings_max": 32, "source_bytes_max": 1_048_576
                    },
                    "relationship_capabilities": [{
                        "direction": "outgoing", "facets": ["calls", "references"], "bounds": {
                            "max_seeds": 8, "max_facets": 4, "max_relationships": 100,
                            "max_occurrences": 100, "max_symbols": 100, "max_sources": 100
                        }
                    }]
                }))
            }))
            .route("/rift/v1/search", post(move |bytes: axum::body::Bytes| {
                let requests = Arc::clone(&requests);
                let expires = endpoint_expiry.clone();
                async move {
                    let request: Value = serde_json::from_slice(&bytes).expect("valid raw relationship request");
                    let facet = request["relationships"]["facets"][0].as_str().unwrap_or_default();
                    let seed = request["relationships"]["seeds"][0].as_str().unwrap_or_default();
                    let (next, number) = match seed {
                        ROOT => (A, 1_u64),
                        A => (B, 2),
                        B => (C, 3),
                        _ => return json_response(&json!({"outcome":"unavailable","reason":"insufficient_coverage"})),
                    };
                    let symbol = |id: &str| json!({
                        "symbol": {"id":id,"name":id.rsplit('/').next(),"language":"rust","kind":"function",
                            "origin":{"source_kind":"synthetic"}}, "declarations":[]
                    });
                    let result = json!({
                        "groups": [{"seed":seed,"complete":true,"relationships":[{
                            "id": format!("{number:064x}"),
                            "relationship":{"from":seed,"to":next,"kind":facet,"facets":[facet],"derivation":"resolution"}
                        }]}],
                        "symbols":[symbol(seed),symbol(next)],
                        "analyzer_revision":"analyzer-v1","corpus_revision":"corpus-v1",
                        "view":{"id":VIEW,"expires_at":expires}
                    });
                    requests.lock().await.push(request);
                    json_response(&result)
                }
            }))
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
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
        service.abort();
        let QueryReadResult::Results(result) = result else {
            return Err(format!(
                "expected captured remote path, received {result:?} after {:?} requests",
                requests_seen.lock().await.as_slice()
            )
            .into());
        };
        assert_eq!(
            result.view.as_ref().map(|view| view.expires_at.as_str()),
            Some(expires.as_str())
        );
        let QueryAnswer::Bindings { bindings, .. } = result.answer else {
            return Err("expected call bindings".into());
        };
        assert_eq!(bindings.len(), 1, "{bindings:?}");
        assert!(
            matches!(&bindings[0]["callee"], rift_protocol::query::RdfTerm::Iri { value } if value == C)
        );
        let requests_seen = requests_seen.lock().await;
        assert_eq!(
            requests_seen[1]["relationships"]["facets"],
            json!(["references"])
        );
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
    async fn graph_query_reads_remote_node_pages_at_the_selected_release() -> TestResult {
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
                "outcome":"found", "revision":RELEASE,
                "symbol":{"id":ROOT,"name":"root","language":"rust","kind":"function","origin":origin},
                "declarations":[{"origin":origin,"unit":UNIT,"range":{"start":0,"end":12},"line":1,
                    "signature_indices":[],"type_indices":[],"documentation_indices":[]}]
            }))
        }))
        .route("/rift/v1/nodes", axum::routing::post(move |bytes: axum::body::Bytes| {
            let observed = Arc::clone(&observed);
            async move {
                let request: Value = serde_json::from_slice(&bytes).expect("exact node request");
                assert_eq!(request["symbol"], ROOT);
                assert_eq!(request["revision"], RELEASE);
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let page = if let Some(cursor) = request["node_cursor"].as_str() {
                    assert_eq!(cursor, "next-node");
                    json!({"symbol":ROOT,"revision":RELEASE,"nodes":[{
                        "declaration":0,"index":1,"unit":UNIT,"language":"rust","kind":"identifier",
                        "range":{"start":3,"end":7},"source":"root","parent":0
                    }]})
                } else {
                    json!({"symbol":ROOT,"revision":RELEASE,"node_cursor":"next-node","nodes":[{
                        "declaration":0,"index":0,"unit":UNIT,"language":"rust","kind":"function_item",
                        "range":{"start":0,"end":12},"source":"fn root() {}"
                    }]})
                };
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
                    "SELECT ?i ?text {{ ?node :of <{ROOT}>; :index ?i; :source ?text }} ORDER BY ?i"
                ),
                language: QueryLanguage::Sparql,
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
        service.abort();
        let QueryReadResult::Results(result) = result else {
            return Err(format!("expected exact node rows, received {result:?}").into());
        };
        let QueryAnswer::Bindings { bindings, .. } = result.answer else {
            return Err("expected exact node bindings".into());
        };
        assert_eq!(pages.load(std::sync::atomic::Ordering::Relaxed), 2);
        let sources = bindings
            .iter()
            .map(|row| match &row["text"] {
                rift_protocol::query::RdfTerm::Literal { value, .. } => value.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>();
        assert_eq!(sources, ["fn root() {}", "root"]);
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
        };
        let result = server
            .graph_query(params)
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
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
        };
        let result = server
            .graph_query(params)
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
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

    #[tokio::test]
    async fn graph_ask_refuses_uncovered_relationships() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(directory.path().join("lib.rs"), "fn alpha() {}\n")?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
        let result = server
            .graph_query(QueryParams {
                query: "ASK { ?a :calls ?b }".to_owned(),
                language: QueryLanguage::Sparql,
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
        assert!(matches!(
            result,
            QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
                reason: SymbolUnavailableReason::InsufficientCoverage,
                ..
            })
        ));
        Ok(())
    }
}
