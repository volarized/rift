//! Retains the complete selected producer pages before graph evaluation.

use crate::parameters::Parameters;
use rift_error::RiftError;
use rift_protocol::read::{PAGE_LIMIT_MAX, ReadWarning, SearchParams, SearchResult};
use rift_protocol::search_read::{SearchReadResult, SearchUnavailable};
use rift_protocol::symbol_read::SymbolUnavailableReason;
use rift_search::sparql::RetrievalSelection;

use super::{RiftMcp, ToolFailure, read_failure, relationships::coverage_failure};

const PRODUCER_PAGES_MAX: usize = 64;

impl RiftMcp {
    pub(super) async fn query_retrieval(
        &self,
        selection: &RetrievalSelection,
        limits: rift_search::sparql::GraphLimits,
    ) -> Result<Result<Box<SearchResult>, SearchUnavailable>, ToolFailure> {
        collect_retrieval(
            selection.request.clone(),
            limits,
            read_failure,
            |request| async move {
                self.search(Parameters(request))
                    .await
                    .map(|result| result.0)
            },
        )
        .await
    }
}

/// Acquires the producer pages selected by one query service and validates their common context.
pub(crate) async fn collect_retrieval<E, F>(
    mut request: SearchParams,
    limits: rift_search::sparql::GraphLimits,
    map_error: impl Fn(RiftError) -> E,
    mut read: impl FnMut(SearchParams) -> F,
) -> Result<Result<Box<SearchResult>, SearchUnavailable>, E>
where
    F: std::future::Future<Output = Result<SearchReadResult, E>>,
{
    let one_page = request.limit.is_some() || request.page_index != 0;
    if !one_page {
        request.limit = Some(PAGE_LIMIT_MAX);
    }
    let mut held: Option<Box<SearchResult>> = None;
    let mut bytes_left = limits.literal_bytes_max();
    for _ in 0..PRODUCER_PAGES_MAX {
        let result = read(request.clone()).await?;
        let mut page = match result {
            SearchReadResult::Results(page) => page,
            SearchReadResult::Unavailable(reason) => return Ok(Err(reason)),
        };
        let failure = if page.warnings.iter().any(incomplete_retrieval) {
            Some(SymbolUnavailableReason::InsufficientCoverage)
        } else if page.pagination.page_index != request.page_index
            || held.as_ref().is_some_and(|held| {
                page.view != held.view || page.pagination.total_pages != held.pagination.total_pages
            })
        {
            Some(SymbolUnavailableReason::ViewContext)
        } else {
            None
        };
        if let Some(reason) = failure {
            return Ok(Err(unavailable(reason, page)));
        }
        let bytes = crate::proxy::encode_bounded(&page, bytes_left)
            .map_err(|_| map_error(coverage_failure()))?;
        bytes_left -= bytes.len();
        let next = request
            .page_index
            .checked_add(1)
            .ok_or_else(|| map_error(coverage_failure()))?;
        let more = !one_page && next < page.pagination.total_pages;
        if more && (page.view.is_none() || page.results.is_empty()) {
            return Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
                page,
            )));
        }
        if let Some(held) = &mut held {
            if held.results.len().saturating_add(page.results.len()) > limits.solutions_max() {
                return Ok(Err(unavailable(
                    SymbolUnavailableReason::InsufficientCoverage,
                    page,
                )));
            }
            held.results.append(&mut page.results);
            for warning in page.warnings {
                if !held.warnings.contains(&warning) {
                    held.warnings.push(warning);
                }
            }
        } else {
            request.view = page.view.as_ref().map(|view| view.id.clone());
            held = Some(page);
        }
        if !more {
            return held.map(Ok).ok_or_else(|| map_error(coverage_failure()));
        }
        request.page_index = next;
    }
    Err(map_error(coverage_failure()))
}

fn unavailable(reason: SymbolUnavailableReason, page: Box<SearchResult>) -> SearchUnavailable {
    SearchUnavailable::Unavailable {
        reason,
        view: page.view,
        warnings: page.warnings,
    }
}

fn incomplete_retrieval(warning: &ReadWarning) -> bool {
    matches!(
        warning,
        ReadWarning::StaleIndex { .. }
            | ReadWarning::LocalIndexPreparing { .. }
            | ReadWarning::VectorIndexPreparing { .. }
            | ReadWarning::VectorRankingUnavailable { .. }
            | ReadWarning::LexicalRankingUnavailable { .. }
            | ReadWarning::LexicalRankingTruncated { .. }
            | ReadWarning::QueryNarrowed { .. }
            | ReadWarning::ResultsTruncated { .. }
            | ReadWarning::PatternMatchesTruncated { .. }
            | ReadWarning::PatternIndexPreparing { .. }
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
