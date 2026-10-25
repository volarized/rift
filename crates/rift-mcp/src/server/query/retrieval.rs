//! Complete producer selections assembled before query evaluation.

use rift_protocol::query::QueryConfiguration;
use rift_protocol::read::{Pagination, SearchParams, SearchResult};
use rift_protocol::search_read::{SearchReadResult, SearchUnavailable};
use rift_protocol::symbol_read::SymbolUnavailableReason;

use super::{
    QueryBudget, RiftMcp, ToolFailure, acquisition_failure, failure, incomplete_retrieval,
};

impl RiftMcp {
    pub(super) async fn complete_retrieval(
        &self,
        request: &SearchParams,
        configuration: &QueryConfiguration,
        requests: &rift_server::ProviderRequests,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
    ) -> Result<SearchReadResult, ToolFailure> {
        if request.page_index != 0 {
            return Err(failure(
                rift_error::errors::server::read_invalid()
                    .field("query")
                    .violation(
                        "use SPARQL LIMIT and OFFSET; retrieval transport starts at page zero",
                    )
                    .error(),
            ));
        }
        let mut paging = request.clone();
        paging.limit = Some(request.limit.unwrap_or(rift_protocol::read::PAGE_LIMIT_MAX));
        let mut selected: Option<Box<SearchResult>> = None;
        loop {
            requests.claim().map_err(failure)?;
            let result = self
                .discovery_search(
                    crate::parameters::Parameters(paging.clone()),
                    rmcp::model::Extensions::default(),
                )
                .await?
                .0;
            let mut page = match result {
                SearchReadResult::Results(page) => page,
                unavailable @ SearchReadResult::Unavailable(_) => return Ok(unavailable),
            };
            if page.warnings.iter().any(incomplete_retrieval) {
                return Ok(unavailable(
                    SymbolUnavailableReason::InsufficientCoverage,
                    *page,
                ));
            }
            if page.pagination.page_index != paging.page_index
                || selected.as_ref().is_some_and(|held| {
                    held.view != page.view
                        || held.pagination.total_pages != page.pagination.total_pages
                })
            {
                return Ok(unavailable(SymbolUnavailableReason::ViewContext, *page));
            }
            page = self
                .query_work(budget, deadline, move |budget| {
                    budget.charge(&page)?;
                    Ok(page)
                })
                .await?;
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
                .as_ref()
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
            match &mut selected {
                Some(held) => {
                    held.results.append(&mut page.results);
                    for warning in page.warnings {
                        if !held.warnings.contains(&warning) {
                            held.warnings.push(warning);
                        }
                    }
                }
                None => {
                    paging.view = page.view.as_ref().map(|view| view.id.clone());
                    selected = Some(page);
                }
            }
            let held = selected.as_mut().ok_or_else(|| {
                failure(acquisition_failure("retrieval selection is unavailable"))
            })?;
            if !more {
                held.pagination = Pagination {
                    page_index: 0,
                    total_pages: u64::from(!held.results.is_empty()),
                };
                return Ok(SearchReadResult::Results(selected.ok_or_else(|| {
                    failure(acquisition_failure("retrieval selection is unavailable"))
                })?));
            }
            paging.page_index = next;
        }
    }
}

fn unavailable(reason: SymbolUnavailableReason, page: SearchResult) -> SearchReadResult {
    SearchReadResult::Unavailable(SearchUnavailable::Unavailable {
        reason,
        view: page.view,
        warnings: page.warnings,
    })
}
