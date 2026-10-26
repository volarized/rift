use super::symbol::{SavedRead, SavedSymbol};
use super::{CandidateResponses, RiftMcp, read_failure, unavailable};
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
        let view = CapturedViewId::parse(&selection.view).map_err(read_failure)?;
        if let Some(captured) = self.captured_query_material(&view, "search")? {
            return self.query_material_page(captured, selection).await;
        }
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

impl RiftMcp {
    pub(in crate::server) fn captured_query_material(
        &self,
        view: &CapturedViewId,
        operation: &str,
    ) -> Result<Option<rift_server::CapturedQueryView>, super::ToolFailure> {
        if !self.has_captured_context(view) {
            return Ok(None);
        }
        let context = self.captured_context(view, operation)?;
        let views = self
            .local_views
            .get()
            .ok_or_else(retention_failure)
            .map_err(read_failure)?;
        match views.query_view(view, std::path::Path::new(&context.root), &context.owner) {
            Ok(captured) => Ok(Some(captured)),
            Err(SymbolUnavailableReason::ViewContext) => Ok(None),
            Err(reason) => Err(super::captured_read_refusal(operation, view, reason)),
        }
    }

    pub(in crate::server) async fn prepare_query_material(
        &self,
        bodies: Vec<(
            crate::server::QueryFactSelection,
            rift_search::sparql::QueryServices,
            Option<rift_search::sparql::QueryIdentities>,
        )>,
        originals: Vec<rift_protocol::read::SearchParams>,
        configuration: &QueryConfiguration,
        budget: &rift_server::QueryFactBudget<'static>,
        deadline: crate::server::RequestDeadline,
    ) -> Result<
        (
            crate::proxy::query_material::Input,
            rift_search::sparql::QueryFacts,
            rift_search::sparql::QueryServices,
            Vec<ReadWarning>,
        ),
        super::ToolFailure,
    > {
        let configuration = configuration.clone();
        let limits =
            rift_search::sparql::QueryLimits::try_from(&configuration).map_err(read_failure)?;
        let maximum = rift_server::QueryProcessLimits::from_query(limits)
            .map_err(read_failure)?
            .material_bytes_max;
        self.query_work(budget, deadline, move |budget| {
            let bytes = bodies
                .len()
                .checked_mul(
                    size_of::<(
                        String,
                        crate::server::QueryFactSelection,
                        rift_search::sparql::QueryServices,
                        Option<rift_search::sparql::QueryIdentities>,
                    )>() + "local".len(),
                )
                .ok_or_else(retention_failure)?;
            budget.charge_material(bytes)?;
            let mut selections = Vec::new();
            selections
                .try_reserve_exact(bodies.len())
                .map_err(|_| retention_failure())?;
            let extra = selections
                .capacity()
                .checked_sub(bodies.len())
                .and_then(|extra| {
                    extra.checked_mul(size_of::<(
                        String,
                        crate::server::QueryFactSelection,
                        rift_search::sparql::QueryServices,
                        Option<rift_search::sparql::QueryIdentities>,
                    )>())
                })
                .ok_or_else(retention_failure)?;
            budget.charge_material(extra)?;
            for (body, services, identities) in bodies {
                budget.check()?;
                selections.push(("local".to_owned(), body, services, identities));
            }
            crate::proxy::prepare_query_services(
                selections,
                &originals,
                limits,
                configuration,
                maximum,
                budget,
            )
        })
        .await
    }

    pub(in crate::server) async fn retain_query_material(
        &self,
        input: crate::proxy::query_material::Input,
        answer: QueryAnswer,
        warnings: Vec<ReadWarning>,
        budget: &rift_server::QueryFactBudget<'static>,
        deadline: crate::server::RequestDeadline,
    ) -> Result<QueryReadResult, super::ToolFailure> {
        let views = Arc::clone(&self.local_views);
        let responses = Arc::clone(&self.candidate_responses);
        self.query_work(budget, deadline, move |budget| {
            let views = views.get().ok_or_else(retention_failure)?;
            let (root, owner) = input.original_context()?;
            budget.charge(&(root, owner))?;
            let root = std::path::PathBuf::from(root);
            let owner = owner.clone();
            budget.charge(&answer)?;
            input.capture_with(views, (&root, &owner), &answer, warnings, budget, |view| {
                responses.save_symbol(&root, &owner, view)
            })
        })
        .await
    }

    async fn query_material_budget(
        &self,
    ) -> Result<
        (
            crate::server::RequestDeadline,
            rift_server::QueryFactBudget<'static>,
        ),
        super::ToolFailure,
    > {
        let deadline = self.request_deadline().await;
        let configuration = self.query_configuration().await;
        let limits =
            rift_search::sparql::QueryLimits::try_from(&configuration).map_err(read_failure)?;
        let budget =
            rift_server::QueryFactBudget::for_request(limits, deadline.at().into_std(), &|| false)
                .map_err(read_failure)?;
        Ok((deadline, budget))
    }

    async fn query_material_page(
        &self,
        captured: rift_server::CapturedQueryView,
        selection: PageSelection,
    ) -> Result<Option<QueryReadResult>, super::ToolFailure> {
        let (deadline, budget) = self.query_material_budget().await?;
        self.query_work(&budget, deadline, move |budget| {
            budget.charge_bytes(captured.material().metadata())?;
            let saved = crate::proxy::query_material::metadata(&captured)?;
            let answer = saved
                .answer
                .decode_bytes(captured.material().records(), Some(selection.page))?;
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
            let response = saved.answer.response(answer, selection.page)?;
            budget.charge(&response)?;
            Ok(Some(response))
        })
        .await
    }

    pub(in crate::server) async fn query_material_source(
        &self,
        params: rift_protocol::source_read::GetSourceParams,
        captured: rift_server::CapturedQueryView,
        context: Option<
            &std::sync::Mutex<Option<crate::global::candidates::SavedCandidateBackend>>,
        >,
    ) -> Result<rift_protocol::source_read::GetSourceResult, super::ToolFailure> {
        let (deadline, budget) = self.query_material_budget().await?;
        let views = Arc::clone(&self.local_views);
        let selected = self
            .query_work(&budget, deadline, move |budget| {
                let views = views.get().ok_or_else(retention_failure)?;
                material_source(params, &captured, views, budget)
            })
            .await?;
        let (request, provider, view, public, global, digest) = match selected {
            MaterialSource::Captured(response) => return Ok(response),
            MaterialSource::Retained(selected) => {
                let RetainedMaterialSource {
                    request,
                    provider,
                    view,
                    public,
                    global,
                    digest,
                } = *selected;
                (request, provider, view, public, global, digest)
            }
        };
        let remote = if let Some(global) = global {
            self.query_global_source_selected(&request, global, digest, context, deadline)
                .await?
        } else {
            None
        };
        let (response, request) = if let Some(response) = remote {
            (response, request)
        } else {
            let views = Arc::clone(&self.local_views);
            self.query_work(&budget, deadline, move |_budget| {
                let views = views.get().ok_or_else(retention_failure)?;
                let mut request = request;
                request.view = Some(provider.view.id);
                let response = views.get_source(
                    &request,
                    std::path::Path::new(&provider.root),
                    &provider.owner,
                )?;
                Ok((response, request))
            })
            .await?
        };
        self.query_work(&budget, deadline, move |budget| {
            budget.charge(&response)?;
            crate::proxy::query_material_read::qualify_source(
                response,
                &request,
                "local",
                view,
                Some(public),
            )
        })
        .await
    }

    pub(in crate::server) async fn query_material_symbol(
        &self,
        params: rift_protocol::symbol_read::GetSymbolParams,
        captured: rift_server::CapturedQueryView,
        context: Option<
            &std::sync::Mutex<Option<crate::global::candidates::SavedCandidateBackend>>,
        >,
    ) -> Result<rift_protocol::symbol_read::GetSymbolResult, super::ToolFailure> {
        let (deadline, budget) = self.query_material_budget().await?;
        let views = Arc::clone(&self.local_views);
        let selected = self
            .query_work(&budget, deadline, move |budget| {
                let views = views.get().ok_or_else(retention_failure)?;
                material_symbol(params, &captured, views, budget)
            })
            .await?;
        let MaterialSymbol {
            request,
            provider,
            view,
            source_units,
            global,
        } = selected;
        let remote = if let Some(global) = global {
            self.query_global_symbol_selected(&request, global, context, deadline)
                .await?
        } else {
            None
        };
        let (response, request) = if let Some(response) = remote {
            (response, request)
        } else {
            let views = Arc::clone(&self.local_views);
            self.query_work(&budget, deadline, move |_budget| {
                let views = views.get().ok_or_else(retention_failure)?;
                let mut request = request;
                request.view = Some(provider.view.id);
                let response = views.resolve(
                    &request,
                    std::path::Path::new(&provider.root),
                    &provider.owner,
                )?;
                Ok((response, request))
            })
            .await?
        };
        self.query_work(&budget, deadline, move |budget| {
            budget.charge(&response)?;
            crate::proxy::query_material_read::qualify_symbol(
                response,
                &request,
                "local",
                view,
                &source_units,
            )
        })
        .await
    }
}

enum MaterialSource {
    Captured(rift_protocol::source_read::GetSourceResult),
    Retained(Box<RetainedMaterialSource>),
}

struct RetainedMaterialSource {
    request: rift_protocol::source_read::GetSourceParams,
    provider: SavedSymbol,
    view: CapturedView,
    public: rift_protocol::read::SourceUnitId,
    global: Option<global::SelectedGlobal>,
    digest: Option<rift_protocol::identity::SourceDigest>,
}

struct MaterialSymbol {
    request: rift_protocol::symbol_read::GetSymbolParams,
    provider: SavedSymbol,
    view: CapturedView,
    source_units: Vec<(
        rift_protocol::read::SourceUnitId,
        rift_protocol::read::SourceUnitId,
    )>,
    global: Option<global::SelectedGlobal>,
}

fn material_provider(
    provider: &crate::proxy::query_material::Provider,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<SavedSymbol, RiftError> {
    if provider.backend != "local" {
        return Err(retention_failure());
    }
    budget.charge(&(&provider.root, &provider.owner, &provider.view))?;
    Ok(SavedSymbol {
        root: provider.root.clone(),
        owner: provider.owner.clone(),
        view: provider.view.clone(),
    })
}

fn material_global(
    provider: &crate::proxy::query_material::Provider,
    context: &SavedSymbol,
    owner: &rift_protocol::identity::SymbolOwner,
    views: &rift_server::LocalSymbolViews,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Option<global::SelectedGlobal>, RiftError> {
    let mut matches = provider
        .upstream
        .iter()
        .filter(|selected| selected.owners.contains(owner));
    let Some(selected) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(retention_failure());
    }
    budget.charge(selected)?;
    budget.charge(context)?;
    let facts = views
        .selected_input(
            &provider.view.id,
            std::path::Path::new(&provider.root),
            &provider.owner,
        )
        .ok()
        .and_then(|material| material.package_facts());
    Ok(Some(global::SelectedGlobal {
        source: selected.clone(),
        root: SavedSymbol {
            root: context.root.clone(),
            owner: context.owner.clone(),
            view: context.view.clone(),
        },
        facts,
    }))
}

fn material_source(
    mut params: rift_protocol::source_read::GetSourceParams,
    captured: &rift_server::CapturedQueryView,
    views: &rift_server::LocalSymbolViews,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<MaterialSource, RiftError> {
    budget.charge_bytes(captured.material().metadata())?;
    let saved = crate::proxy::query_material::metadata(captured)?;
    let source = crate::proxy::query_material_read::selected_source(&saved, &params.unit)?;
    if let Some(bytes) = &source.bytes {
        let bytes = captured
            .material()
            .sources()
            .get(bytes.clone())
            .ok_or_else(retention_failure)?;
        return crate::proxy::query_material_read::captured_source(
            params,
            bytes,
            captured.view(),
            budget,
        )
        .map(MaterialSource::Captured);
    }
    budget.charge(&(
        &source.original,
        &params.unit,
        &source.digest,
        captured.view(),
    ))?;
    let provider = saved
        .providers
        .get(source.provider)
        .ok_or_else(retention_failure)?;
    let (owner, _) = rift_protocol::identity::parse_source_unit_identity(source.original.as_str())?;
    let global = match owner {
        Some(owner) => material_global(provider, &saved.answer.context, &owner, views, budget)?,
        None => None,
    };
    let provider = material_provider(provider, budget)?;
    let public = std::mem::replace(&mut params.unit, source.original.clone());
    budget.charge_material(size_of::<RetainedMaterialSource>())?;
    Ok(MaterialSource::Retained(Box::new(RetainedMaterialSource {
        request: params,
        provider,
        view: captured.view().clone(),
        public,
        global,
        digest: source.digest.clone(),
    })))
}

fn material_symbol(
    mut params: rift_protocol::symbol_read::GetSymbolParams,
    captured: &rift_server::CapturedQueryView,
    views: &rift_server::LocalSymbolViews,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<MaterialSymbol, RiftError> {
    budget.charge_bytes(captured.material().metadata())?;
    let saved = crate::proxy::query_material::metadata(captured)?;
    let (index, original) =
        crate::proxy::query_material_read::selected_symbol(&saved, params.id.as_str())?;
    let provider = saved.providers.get(index).ok_or_else(retention_failure)?;
    budget.charge(&(original, captured.view()))?;
    let owner = rift_protocol::identity::SymbolIdentity::parse(original.as_str())?
        .owner()
        .clone();
    let global = material_global(provider, &saved.answer.context, &owner, views, budget)?;
    let provider = material_provider(provider, budget)?;
    params.id = original.clone();
    let count = saved
        .sources
        .iter()
        .filter(|source| source.provider == index)
        .count();
    budget.charge_material(
        count
            .checked_mul(size_of::<(
                rift_protocol::read::SourceUnitId,
                rift_protocol::read::SourceUnitId,
            )>())
            .ok_or_else(retention_failure)?,
    )?;
    let mut source_units = Vec::new();
    source_units
        .try_reserve_exact(count)
        .map_err(|_| retention_failure())?;
    let extra = source_units
        .capacity()
        .checked_sub(count)
        .and_then(|extra| {
            extra.checked_mul(size_of::<(
                rift_protocol::read::SourceUnitId,
                rift_protocol::read::SourceUnitId,
            )>())
        })
        .ok_or_else(retention_failure)?;
    budget.charge_material(extra)?;
    for source in saved
        .sources
        .iter()
        .filter(|source| source.provider == index)
    {
        budget.check()?;
        budget.charge(&(&source.original, &source.public))?;
        source_units.push((source.original.clone(), source.public.clone()));
    }
    Ok(MaterialSymbol {
        request: params,
        provider,
        view: captured.view().clone(),
        source_units,
        global,
    })
}

#[cfg(test)]
mod material_tests {
    use rift_protocol::configuration::{ServerConfiguration, WorkspaceConfiguration};
    use rift_protocol::identity::SymbolOwner;
    use rift_protocol::query::{QueryAnswer, QueryConfiguration, QueryReadResult};
    use rift_protocol::symbol_read::CapturedView;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn material_input(
        root: &std::path::Path,
        original: &CapturedView,
        selection: &rift_search::query::QueryProviderSelection,
        budget: &mut rift_server::QueryFactBudget<'_>,
    ) -> TestResult<crate::proxy::query_material::Input> {
        let configuration = QueryConfiguration::default();
        let limits = rift_search::sparql::QueryLimits::try_from(&configuration)?;
        let maximum = rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max;
        let body = crate::server::QueryFactSelection {
            root: root.to_str().ok_or("UTF-8 workspace")?.to_owned(),
            owner: SymbolOwner::Local,
            view: original.clone(),
            selection: selection.clone(),
            requests: 0,
            graph: Some(rift_search::sparql::LOCAL_GRAPH.to_owned()),
            requirements: rift_search::sparql::FactRequirements::default(),
            subjects: Vec::new(),
            facts: rift_search::sparql::QueryFacts::default(),
            services: rift_search::sparql::QueryServices::default(),
            warnings: Vec::new(),
            upstream: Vec::new(),
        };
        let (input, _, _, _) = crate::proxy::prepare_query_services(
            vec![(
                "local".to_owned(),
                body,
                rift_search::sparql::QueryServices::default(),
                None,
            )],
            &[],
            limits,
            configuration,
            maximum,
            budget,
        )?;
        Ok(input)
    }

    #[test]
    fn query_material_callback_refusal_keeps_previous_actor_authority() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        let reads = rift_server::ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )?;
        let material = reads.local_search_view_input(true)?;
        let root = material.root().to_path_buf();
        let selection = material.provider_selection(&WorkspaceConfiguration::default())?;
        let views = rift_server::LocalSymbolViews::new(1 << 20, [73; 32]);
        let original = views
            .capture(material.with_provider_selection(selection.clone())?)?
            .ok_or("original local view")?;
        let responses = super::super::CandidateResponses::for_configuration(&ServerConfiguration {
            retained_views: 1,
            ..ServerConfiguration::default()
        });
        let limits = rift_search::sparql::QueryLimits::try_from(&QueryConfiguration::default())?;
        let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
        let first = material_input(&root, &original, &selection, &mut budget)?.capture_with(
            &views,
            (&root, &SymbolOwner::Local),
            &QueryAnswer::Boolean { value: true },
            Vec::new(),
            &mut budget,
            |view| responses.save_symbol(&root, &SymbolOwner::Local, view),
        )?;
        let QueryReadResult::Results(first) = first else {
            return Err("first query must be admitted".into());
        };
        let first = first.view.ok_or("first query view")?;
        let before = {
            let rows = responses.bytes.lock().expect("candidate responses");
            assert_eq!(rows.len(), 1);
            (rows[0].id.clone(), rows[0].expires, rows[0].bytes.clone())
        };
        let mut attempted = None;
        let refusal = material_input(&root, &original, &selection, &mut budget)?.capture_with(
            &views,
            (&root, &SymbolOwner::Local),
            &QueryAnswer::Boolean { value: false },
            Vec::new(),
            &mut budget,
            |view| {
                attempted = Some(view.clone());
                responses.save_symbol(&root, &SymbolOwner::Local, view)
            },
        );
        assert!(refusal.is_err());
        let attempted = attempted.ok_or("actor admission callback must run")?;
        assert_ne!(attempted.id, first.id);
        assert!(
            views
                .query_view(&attempted.id, &root, &SymbolOwner::Local)
                .is_err()
        );
        let held = views
            .query_view(&first.id, &root, &SymbolOwner::Local)
            .map_err(|reason| format!("previous query authority: {reason:?}"))?;
        assert_eq!(held.view(), &first);
        assert!(
            views
                .selected_input(&original.id, &root, &SymbolOwner::Local)
                .is_ok()
        );
        let rows = responses.bytes.lock().expect("candidate responses");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].id.clone(), rows[0].expires, rows[0].bytes.clone()),
            before
        );
        Ok(())
    }
}
