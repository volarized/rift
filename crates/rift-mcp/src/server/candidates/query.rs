use super::symbol::{SavedRead, SavedSymbol};
use super::{BYTES_MAX, CandidateResponses, RiftMcp, read_failure, unavailable};
use rift_error::{RiftError, errors};
use rift_protocol::query::{QueryAnswer, QueryConfiguration, QueryReadResult, QueryResult};
use rift_protocol::read::{Pagination, ReadWarning};
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use rift_search::sparql::{PageForm, PageSelection, continuation_query};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Deserialize, Serialize)]
pub(crate) struct SavedQuery {
    pub(crate) context: SavedSymbol,
    template_end: usize,
    pages: Vec<QueryPage>,
    warnings: Vec<ReadWarning>,
    #[serde(default)]
    upstream: Vec<QueryGlobalSelection>,
}

mod global;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryGlobalSelection {
    pub(crate) owners: Vec<rift_protocol::identity::SymbolOwner>,
    pub(crate) view: CapturedView,
    pub(crate) configuration: rift_protocol::configuration::GlobalConfiguration,
}

#[derive(Deserialize, Serialize)]
struct QueryPage {
    start: usize,
    end: usize,
}

pub(in crate::server) struct RetainedQuery {
    pub(in crate::server) material: rift_server::LocalSymbolViewInput,
    pub(in crate::server) answer: QueryAnswer,
    pub(in crate::server) warnings: Vec<ReadWarning>,
    pub(in crate::server) upstream: Vec<QueryGlobalSelection>,
}

impl RiftMcp {
    pub(in crate::server) fn query_selection(
        &self,
        view: &CapturedView,
    ) -> Result<
        Result<Option<rift_server::LocalSymbolViewInput>, SymbolUnavailableReason>,
        super::ToolFailure,
    > {
        if !self.has_captured_context(&view.id) {
            return Ok(Ok(None));
        }
        let selected = self.captured_context(&view.id, "query retrieval")?;
        let views = self
            .local_views
            .get()
            .ok_or_else(|| unavailable("retained read material is unavailable"))?;
        Ok(views
            .selected_input(
                &view.id,
                std::path::Path::new(&selected.root),
                &selected.owner,
            )
            .map(Some))
    }

    pub(in crate::server) async fn retain_query(
        &self,
        material: rift_server::LocalSymbolViewInput,
        answer: QueryAnswer,
        configuration: &QueryConfiguration,
        warnings: Vec<ReadWarning>,
        upstream: Vec<QueryGlobalSelection>,
    ) -> Result<QueryReadResult, super::ToolFailure> {
        let views = Arc::clone(&self.local_views);
        let responses = Arc::clone(&self.candidate_responses);
        let configuration = configuration.clone();
        self.blocking
            .run("query retention", move || {
                let views = views.get().ok_or_else(retention_failure)?;
                retain(
                    views,
                    &responses,
                    material,
                    &answer,
                    &configuration,
                    warnings,
                    upstream,
                )
            })
            .await
            .map_err(read_failure)
    }

    pub(in crate::server) async fn retained_query(
        &self,
        view: &CapturedViewId,
        page: Option<u64>,
        budget: rift_server::QueryFactBudget<'static>,
        deadline: crate::server::RequestDeadline,
    ) -> Result<Result<RetainedQuery, SymbolUnavailableReason>, super::ToolFailure> {
        let responses = Arc::clone(&self.candidate_responses);
        let views = Arc::clone(&self.local_views);
        let view = view.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let result = tokio::time::timeout_at(
            deadline.at(),
            self.blocking.run_with_cancellation(
                "retained query result",
                cancellation,
                move |cancelled| {
                    let cancelled = || cancelled.is_cancelled();
                    let mut budget = budget.with_cancellation(&cancelled);
                    budget.check()?;
                    let (saved, material) = match retained(&responses, &views, &view)? {
                        Ok(value) => value,
                        Err(reason) => return Ok(Err(reason)),
                    };
                    if let Some(bytes) = material.query_result() {
                        budget.charge_bytes(&bytes)?;
                    }
                    let answer = saved.decode(&material, page)?;
                    budget.check()?;
                    Ok(Ok(RetainedQuery {
                        material,
                        answer,
                        warnings: saved.warnings,
                        upstream: saved.upstream,
                    }))
                },
            ),
        )
        .await
        .map_err(|_| {
            read_failure(crate::server::query::acquisition_failure(
                "query acquisition exceeded its deadline",
            ))
        })?
        .map_err(read_failure);
        drop(guard);
        result
    }

    pub(in crate::server) async fn retained_query_page(
        &self,
        selection: PageSelection,
    ) -> Result<Option<QueryReadResult>, super::ToolFailure> {
        let responses = Arc::clone(&self.candidate_responses);
        let views = Arc::clone(&self.local_views);
        self.blocking
            .run("retained query page", move || {
                let view = CapturedViewId::parse(&selection.view)?;
                let (saved, material) = match retained(&responses, &views, &view)? {
                    Ok(value) => value,
                    Err(reason) => return Ok(Some(query_unavailable(reason))),
                };
                let answer = saved.decode(&material, Some(selection.page))?;
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
                if !matches {
                    return Ok(None);
                }
                Ok(Some(saved.response(answer, selection.page)?))
            })
            .await
            .map_err(read_failure)
    }
}

fn retained(
    responses: &CandidateResponses,
    views: &std::sync::OnceLock<rift_server::LocalSymbolViews>,
    view: &CapturedViewId,
) -> Result<
    Result<(Box<SavedQuery>, rift_server::LocalSymbolViewInput), SymbolUnavailableReason>,
    RiftError,
> {
    let saved = match responses.query(view)? {
        Ok(saved) => saved,
        Err(reason) => return Ok(Err(reason)),
    };
    let views = views.get().ok_or_else(retention_failure)?;
    let material = match views.selected_input(
        view,
        std::path::Path::new(&saved.context.root),
        &saved.context.owner,
    ) {
        Ok(material) => material,
        Err(reason) => return Ok(Err(reason)),
    };
    Ok(Ok((saved, material)))
}

impl CandidateResponses {
    fn query(
        &self,
        view: &CapturedViewId,
    ) -> Result<Result<Box<SavedQuery>, SymbolUnavailableReason>, RiftError> {
        let rows = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(row) = rows.iter().find(|row| row.id == view.as_str()) else {
            return Ok(Err(SymbolUnavailableReason::ViewContext));
        };
        if row.expires <= SystemTime::now() {
            return Ok(Err(SymbolUnavailableReason::ViewExpired));
        }
        let saved: SavedRead = serde_json::from_slice(&row.bytes).map_err(encoding_failure)?;
        drop(rows);
        Ok(match saved {
            SavedRead::Query(saved) => Ok(saved),
            _ => Err(SymbolUnavailableReason::ViewContext),
        })
    }
}

fn retain(
    views: &rift_server::LocalSymbolViews,
    responses: &CandidateResponses,
    material: rift_server::LocalSymbolViewInput,
    answer: &QueryAnswer,
    configuration: &QueryConfiguration,
    warnings: Vec<ReadWarning>,
    upstream: Vec<QueryGlobalSelection>,
) -> Result<QueryReadResult, RiftError> {
    let maximum = usize::try_from(configuration.result_size.bytes()).map_err(|_| page_failure())?;
    let (encoded, template_end, offsets) = encode_records(answer, maximum)?;
    let material = material.with_query_result(encoded);
    let root = material
        .root()
        .to_str()
        .ok_or_else(retention_failure)?
        .to_owned();
    let owner = material.owner().clone();
    let mut selected = None;
    views.capture_with(material, |view| {
        let mut saved = SavedQuery {
            context: SavedSymbol {
                root,
                owner,
                view: view.clone(),
            },
            template_end,
            pages: Vec::new(),
            warnings,
            upstream,
        };
        saved.pages = plan_pages(answer, &offsets, &saved, configuration)?;
        let response = saved.response(first_page(answer, saved.pages.first(), &offsets), 0)?;
        let encoded = crate::proxy::encode_bounded(&SavedRead::Query(Box::new(saved)), BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        responses.save_bytes(
            view.id.as_str().to_owned(),
            crate::proxy::captured_expiry(view)?,
            encoded,
        )?;
        selected = Some(response);
        Ok(true)
    })?;
    Ok(
        selected
            .unwrap_or_else(|| query_unavailable(SymbolUnavailableReason::InsufficientCoverage)),
    )
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
        view: Some(saved.context.view.clone()),
        pagination: Pagination {
            page_index: u64::MAX,
            total_pages: u64::MAX,
        }
        .into(),
        warnings: saved.warnings.clone(),
    };
    envelope.pagination.next_query = Some(continuation_query(
        answer,
        &saved.context.view.id,
        u64::MAX,
    )?);
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
    pub(crate) fn at_view(mut self, view: CapturedView) -> Self {
        self.context.view = view;
        self
    }

    pub(crate) fn validate_pages(
        &self,
        bytes: &[u8],
        maximum: usize,
        check: impl Fn() -> Result<(), RiftError>,
    ) -> Result<(), RiftError> {
        for page in 0..self.pages.len().max(1) {
            check()?;
            let page = u64::try_from(page).map_err(|_| page_failure())?;
            let response = self.response(self.decode_bytes(bytes, Some(page))?, page)?;
            if encoded_size(&response)? > maximum {
                return Err(page_failure());
            }
        }
        check()
    }

    pub(crate) fn encoded(
        context: SavedSymbol,
        answer: &QueryAnswer,
        configuration: &QueryConfiguration,
        warnings: Vec<ReadWarning>,
        upstream: Vec<QueryGlobalSelection>,
    ) -> Result<(Self, Vec<u8>, QueryReadResult), RiftError> {
        let maximum =
            usize::try_from(configuration.result_size.bytes()).map_err(|_| page_failure())?;
        let (bytes, template_end, offsets) = encode_records(answer, maximum)?;
        let mut saved = Self {
            context,
            template_end,
            pages: Vec::new(),
            warnings,
            upstream,
        };
        saved.pages = plan_pages(answer, &offsets, &saved, configuration)?;
        let response = saved.response(first_page(answer, saved.pages.first(), &offsets), 0)?;
        Ok((saved, bytes, response))
    }

    fn decode(
        &self,
        material: &rift_server::LocalSymbolViewInput,
        page: Option<u64>,
    ) -> Result<QueryAnswer, RiftError> {
        let bytes = material.query_result().ok_or_else(retention_failure)?;
        self.decode_bytes(&bytes, page)
    }

    pub(crate) fn decode_bytes(
        &self,
        bytes: &[u8],
        page: Option<u64>,
    ) -> Result<QueryAnswer, RiftError> {
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

    pub(crate) fn response(
        &self,
        answer: QueryAnswer,
        page_index: u64,
    ) -> Result<QueryReadResult, RiftError> {
        let total_pages = u64::try_from(self.pages.len()).map_err(|_| page_failure())?;
        let next = page_index.checked_add(1).filter(|next| *next < total_pages);
        let mut pagination = rift_protocol::query::QueryPagination::from(Pagination {
            page_index,
            total_pages,
        });
        pagination.next_query = next
            .map(|next| continuation_query(&answer, &self.context.view.id, next))
            .transpose()?;
        Ok(QueryReadResult::Results(Box::new(QueryResult {
            answer,
            view: Some(self.context.view.clone()),
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
