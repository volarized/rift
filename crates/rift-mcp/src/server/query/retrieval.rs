//! Complete producer selections assembled before query evaluation.

use rift_protocol::query::QueryConfiguration;
use rift_protocol::read::{Pagination, SearchParams, SearchResult};
use rift_protocol::search_read::{SearchReadResult, SearchUnavailable};
use rift_protocol::symbol_read::SymbolUnavailableReason;

use super::{
    QueryBudget, RiftMcp, ToolFailure, acquisition_failure, failure, incomplete_retrieval,
};

mod snapshot;
mod target;

impl RiftMcp {
    pub(super) async fn complete_retrieval(
        &self,
        request: &SearchParams,
        configuration: &QueryConfiguration,
        requests: &rift_server::ProviderRequests,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
        selected_views: &mut super::SelectedQueryServices,
    ) -> Result<(SearchReadResult, Vec<Vec<rift_core::ContributionKey>>), ToolFailure> {
        Box::pin(async {
            validate_retrieval_start(request)?;
            let mut paging = request.clone();
            paging.limit = Some(request.limit.unwrap_or(rift_protocol::read::PAGE_LIMIT_MAX));
            let mut selected: Option<Box<SearchResult>> = None;
            let mut contributions = Vec::new();
            let mut snapshot = self.retrieval_snapshot(request).await?;
            loop {
                requests.claim().map_err(failure)?;
                let captured = crate::global::CapturedGlobalViews::default();
                let (result, mut page_contributions) = self
                    .retrieval_page(&paging, &mut snapshot, &captured, budget, deadline)
                    .await?;
                let mut page = match result {
                    SearchReadResult::Results(page) => page,
                    unavailable @ SearchReadResult::Unavailable(_) => {
                        return Ok((unavailable, Vec::new()));
                    }
                };
                if page.warnings.iter().any(incomplete_retrieval) {
                    return Ok((
                        unavailable(SymbolUnavailableReason::InsufficientCoverage, *page),
                        Vec::new(),
                    ));
                }
                if page.pagination.page_index != paging.page_index
                    || selected.as_ref().is_some_and(|held| {
                        held.view != page.view
                            || held.pagination.total_pages != page.pagination.total_pages
                    })
                {
                    return Ok((
                        unavailable(SymbolUnavailableReason::ViewContext, *page),
                        Vec::new(),
                    ));
                }
                let original_view = captured
                    .0
                    .lock()
                    .map_err(|_| {
                        failure(acquisition_failure("producer publication is unavailable"))
                    })?
                    .take();
                let (admitted, original_view) = self
                    .query_work(budget, deadline, move |budget| {
                        budget.charge(&page)?;
                        budget.charge(&original_view)?;
                        Ok((page, original_view))
                    })
                    .await?;
                page = admitted;
                if let Some(view) = original_view {
                    selected_views.include_view(&view)?;
                }
                let (next, more) = retrieval_continuation(
                    &paging,
                    selected.as_deref(),
                    &page,
                    configuration,
                    page_contributions.len(),
                )?;
                contributions.append(&mut page_contributions);
                if let Some(held) = &mut selected {
                    held.results.append(&mut page.results);
                    for warning in page.warnings {
                        if !held.warnings.contains(&warning) {
                            held.warnings.push(warning);
                        }
                    }
                } else {
                    paging.view = page.view.as_ref().map(|view| view.id.clone());
                    selected = Some(page);
                }
                let held = selected.as_mut().ok_or_else(|| {
                    failure(acquisition_failure("retrieval selection is unavailable"))
                })?;
                if !more {
                    held.pagination = Pagination {
                        page_index: 0,
                        total_pages: u64::from(!held.results.is_empty()),
                    };
                    return Ok((
                        SearchReadResult::Results(selected.ok_or_else(|| {
                            failure(acquisition_failure("retrieval selection is unavailable"))
                        })?),
                        contributions,
                    ));
                }
                paging.page_index = next;
            }
        })
        .await
    }
    async fn retrieval_page(
        &self,
        paging: &SearchParams,
        snapshot: &mut Option<snapshot::Snapshot>,
        captured: &crate::global::CapturedGlobalViews,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
    ) -> Result<(SearchReadResult, Vec<Vec<rift_core::ContributionKey>>), ToolFailure> {
        Ok(if let Some(snapshot) = snapshot {
            let (result, contributions) = self
                .retrieval_snapshot_page(snapshot, paging.clone(), budget, deadline)
                .await?;
            (SearchReadResult::Results(result), contributions)
        } else {
            let mut extensions = rmcp::model::Extensions::default();
            extensions.insert(captured.clone());
            let result = self
                .discovery_search(crate::parameters::Parameters(paging.clone()), extensions)
                .await?
                .0;
            let contributions = match &result {
                SearchReadResult::Results(result) => {
                    (0..result.results.len()).map(|_| Vec::new()).collect()
                }
                SearchReadResult::Unavailable(_) => Vec::new(),
            };
            (result, contributions)
        })
    }
}

fn validate_retrieval_start(request: &SearchParams) -> Result<(), ToolFailure> {
    if request.page_index != 0 {
        return Err(failure(
            rift_error::errors::server::read_invalid()
                .field("query")
                .violation("use SPARQL LIMIT and OFFSET; retrieval transport starts at page zero")
                .error(),
        ));
    }
    Ok(())
}

fn retrieval_continuation(
    paging: &SearchParams,
    selected: Option<&SearchResult>,
    page: &SearchResult,
    configuration: &QueryConfiguration,
    contributions_len: usize,
) -> Result<(u64, bool), ToolFailure> {
    let next = paging.page_index.checked_add(1).ok_or_else(|| {
        failure(acquisition_failure(
            "retrieval page exceeds the platform bound",
        ))
    })?;
    let more = next < page.pagination.total_pages;
    if more && (page.view.is_none() || page.results.is_empty()) {
        return Err(failure(acquisition_failure(
            "retrieval page cannot continue without its original context and rows",
        )));
    }
    let count = selected
        .map_or(0, |held| held.results.len())
        .checked_add(page.results.len());
    if count
        .and_then(|count| u64::try_from(count).ok())
        .is_none_or(|count| count > configuration.results)
    {
        return Err(failure(acquisition_failure(
            "retrieval acquisition exceeds search.query.results",
        )));
    }
    if contributions_len != page.results.len() {
        return Err(failure(acquisition_failure(
            "retrieval record associations do not match selected rows",
        )));
    }
    Ok((next, more))
}

fn unavailable(reason: SymbolUnavailableReason, page: SearchResult) -> SearchReadResult {
    SearchReadResult::Unavailable(SearchUnavailable::Unavailable {
        reason,
        view: page.view,
        warnings: page.warnings,
    })
}
