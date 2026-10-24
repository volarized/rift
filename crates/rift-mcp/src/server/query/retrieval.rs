//! Complete producer selections assembled before query evaluation.

use rift_protocol::query::QueryConfiguration;
use rift_protocol::read::{Pagination, SearchParams, SearchResult};
use rift_protocol::search_read::{SearchReadResult, SearchUnavailable};
use rift_protocol::symbol_read::SymbolUnavailableReason;

use super::{RiftMcp, ToolFailure, acquisition_failure, failure, incomplete_retrieval};

impl RiftMcp {
    pub(super) async fn complete_retrieval(
        &self,
        request: &SearchParams,
        configuration: &QueryConfiguration,
        requests: &mut u64,
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
        let mut bytes = 0_u64;
        loop {
            *requests = requests
                .checked_add(1)
                .filter(|count| *count <= configuration.requests)
                .ok_or_else(|| {
                    failure(acquisition_failure(
                        "retrieval acquisition exceeds search.query.requests",
                    ))
                })?;
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
            let (admitted, size) = self
                .blocking
                .run("query retrieval page", move || {
                    let size = page.results.iter().try_fold(0_u64, |size, hit| {
                        let encoded = serde_json::to_vec(hit).map_err(|source| {
                            rift_error::errors::mcp::forward_failed()
                                .source(source)
                                .error()
                        })?;
                        let next = u64::try_from(encoded.len())
                            .ok()
                            .and_then(|bytes| size.checked_add(bytes))
                            .ok_or_else(|| {
                                acquisition_failure("retrieval bytes exceed the platform bound")
                            })?;
                        Ok::<_, rift_error::RiftError>(next)
                    })?;
                    Ok((page, size))
                })
                .await
                .map_err(failure)?;
            page = admitted;
            bytes = bytes
                .checked_add(size)
                .filter(|bytes| *bytes <= configuration.fact_size.bytes())
                .ok_or_else(|| {
                    failure(acquisition_failure(
                        "retrieval acquisition exceeds search.query.fact_size",
                    ))
                })?;
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
            if u64::try_from(held.results.len()).map_or(true, |count| count > configuration.results)
            {
                return Err(failure(acquisition_failure(
                    "retrieval acquisition exceeds search.query.results",
                )));
            }
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
