//! Retains the complete selected producer pages before graph evaluation.

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
        collect_retrieval(selection.request.clone(), limits, read_failure, |request| {
            self.graph_search(request)
        })
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
        let failure = if page.pagination.page_index != request.page_index
            || held.as_ref().is_some_and(|held| {
                page.view != held.view || page.pagination.total_pages != held.pagination.total_pages
            }) {
            Some(SymbolUnavailableReason::ViewContext)
        } else {
            retrieval_coverage(
                &page.warnings,
                request.view.is_none() && request.revision.is_none(),
            )
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

fn retrieval_coverage(warnings: &[ReadWarning], current: bool) -> Option<SymbolUnavailableReason> {
    let mut preparing = false;
    for warning in warnings
        .iter()
        .filter(|warning| incomplete_retrieval(warning))
    {
        if current
            && matches!(
                warning,
                ReadWarning::LocalIndexPreparing { .. } | ReadWarning::PatternIndexPreparing { .. }
            )
        {
            preparing = true;
        } else {
            return Some(SymbolUnavailableReason::InsufficientCoverage);
        }
    }
    preparing.then_some(SymbolUnavailableReason::IndexPreparing)
}

fn incomplete_retrieval(warning: &ReadWarning) -> bool {
    if let ReadWarning::PackageContextDegraded { resolver, .. } = warning {
        return resolver.split('/').next() != Some(rift_dependency::STANDARD_LIBRARY_MANAGER);
    }
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
    )
}

#[cfg(test)]
mod tests {
    use super::{ReadWarning, SymbolUnavailableReason, incomplete_retrieval, retrieval_coverage};

    #[test]
    fn pattern_preparation_is_retryable_only_for_current_reads() {
        let preparing = ReadWarning::PatternIndexPreparing {
            prepared: 1,
            total: 2,
            detail: "fixture".to_owned(),
        };
        assert_eq!(
            retrieval_coverage(std::slice::from_ref(&preparing), true),
            Some(SymbolUnavailableReason::IndexPreparing)
        );
        assert_eq!(
            retrieval_coverage(std::slice::from_ref(&preparing), false),
            Some(SymbolUnavailableReason::InsufficientCoverage)
        );
        assert_eq!(
            retrieval_coverage(
                &[
                    preparing,
                    ReadWarning::LexicalRankingTruncated { matches_max: 1 }
                ],
                true,
            ),
            Some(SymbolUnavailableReason::InsufficientCoverage)
        );
    }

    #[test]
    fn runtime_probe_does_not_invalidate_ranked_package_coverage() {
        for (resolver, incomplete) in [("stdlib/python", false), ("stdlib", false), ("uv", true)] {
            let warning = ReadWarning::PackageContextDegraded {
                resolver: resolver.to_owned(),
                reason: "fixture context unavailable".to_owned(),
            };
            assert_eq!(incomplete_retrieval(&warning), incomplete, "{resolver}");
        }
    }
}
