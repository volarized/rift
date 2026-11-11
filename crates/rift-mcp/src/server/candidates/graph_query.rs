//! Graph query execution and retention under the captured workspace owners.

use std::sync::Arc;
use std::time::SystemTime;

use rift_error::{RiftError, errors};
use rift_protocol::query::{
    QueryAnswer, QueryConfiguration, QueryParams, QueryReadResult, QueryResult,
};
use rift_protocol::read::{Pagination, ReadWarning};
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use rift_search::sparql::{
    GraphLimits, GraphQuery, PageForm, PageSelection, QueryCancellation, continuation_query,
};
use serde::{Deserialize, Serialize};

use super::{BYTES_MAX, CandidateResponses, RiftMcp, read_failure};
use crate::output::{Json, ToolFailure};

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
            let responses = Arc::clone(&self.candidate_responses);
            let views = Arc::clone(&self.local_views);
            return self
                .blocking
                .run("retained graph query", move || {
                    let result = responses.replay_query(&selection)?;
                    let (saved, answer) = match result {
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
                .map(Json)
                .map_err(read_failure);
        }

        let resolved = self
            .published_workspace(super::super::wire::ErrorPhase::Read, deadline)
            .await?;
        resolved
            .published
            .configuration
            .accepted_for_tool(super::super::wire::ErrorPhase::Read)?;
        if resolved.published.preparation.is_some() {
            return Ok(Json(query_unavailable(
                SymbolUnavailableReason::IndexPreparing,
            )));
        }
        if query.symbol_selection().is_none() {
            return Ok(Json(query_unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
        }
        let configuration = resolved
            .published
            .configuration
            .search_configuration()
            .query;
        let reads = Arc::clone(&resolved.published.reads);
        let views = Arc::clone(&self.local_views);
        let responses = Arc::clone(&self.candidate_responses);
        let warnings = resolved.stale.into_iter().collect();
        let cancellation = QueryCancellation::new();
        let _guard = QueryGuard(cancellation.clone());
        let result = tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run("graph query", move || {
                let input = reads.local_search_view_input(true)?;
                let selection = query.symbol_selection().ok_or_else(retention_failure)?;
                let dataset = input.project_symbols(
                    selection,
                    limits,
                    &cancellation,
                    deadline.at().into_std(),
                )?;
                let answer: QueryAnswer = dataset.evaluate(&query, &cancellation)?.into();
                let views = views.get().ok_or_else(retention_failure)?;
                let root = input.root().to_owned();
                let owner = input.owner().clone();
                let mut response = None;
                views.capture_with(input, |view| {
                    if cancellation.is_cancelled() {
                        return errors::search::query_cancelled().fail();
                    }
                    let (saved, bytes, page) = SavedQuery::encoded(
                        &root,
                        &owner,
                        view,
                        &answer,
                        &configuration,
                        warnings,
                    )?;
                    let maximum = usize::try_from(configuration.page_limit.bytes())
                        .map_err(|_| page_failure())?;
                    if encoded_size(&page)? > maximum {
                        return Err(page_failure());
                    }
                    responses.save_query(&saved, &bytes)?;
                    response = Some(page);
                    Ok(true)
                })?;
                response.ok_or_else(retention_failure)
            }),
        )
        .await
        .map_err(|_| read_failure(deadline_failure(deadline.budget())))?
        .map_err(read_failure)?;
        Ok(Json(result))
    }
}

impl CandidateResponses {
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
fn query_unavailable(reason: SymbolUnavailableReason) -> QueryReadResult {
    QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
        reason,
        view: None,
        warnings: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
                language: Default::default(),
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
                language: Default::default(),
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
                language: Default::default(),
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
