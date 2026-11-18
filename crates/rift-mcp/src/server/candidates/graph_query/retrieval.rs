//! Retains the complete selected producer pages before graph evaluation.

use rift_error::RiftError;
use rift_protocol::read::{PAGE_LIMIT_MAX, ReadWarning, SearchParams, SearchResult};
use rift_protocol::search_read::{SearchReadResult, SearchUnavailable};
use rift_protocol::symbol_read::SymbolUnavailableReason;
use rift_search::sparql::RetrievalSelection;

use super::{RiftMcp, ToolFailure, edges::coverage_failure, read_failure};

const PRODUCER_PAGES_MAX: usize = 64;

impl RiftMcp {
    pub(super) async fn query_retrieval(
        &self,
        selection: &RetrievalSelection,
        limits: rift_search::sparql::GraphLimits,
    ) -> Result<Result<Box<SearchResult>, SearchUnavailable>, ToolFailure> {
        let coverage = if selection.flow_limit.is_some() {
            RetrievalCoverage::LexicalFallback
        } else {
            RetrievalCoverage::Complete
        };
        collect_with_coverage(
            selection.request.clone(),
            limits,
            coverage,
            read_failure,
            |request| self.graph_search(request),
        )
        .await
    }
}

/// Acquires the producer pages selected by one query service and validates their common context.
pub(crate) async fn collect_retrieval<E, F>(
    request: SearchParams,
    limits: rift_search::sparql::GraphLimits,
    map_error: impl Fn(RiftError) -> E,
    read: impl FnMut(SearchParams) -> F,
) -> Result<Result<Box<SearchResult>, SearchUnavailable>, E>
where
    F: std::future::Future<Output = Result<SearchReadResult, E>>,
{
    collect_with_coverage(
        request,
        limits,
        RetrievalCoverage::Complete,
        map_error,
        read,
    )
    .await
}

#[derive(Clone, Copy)]
enum RetrievalCoverage {
    Complete,
    LexicalFallback,
}

async fn collect_with_coverage<E, F>(
    mut request: SearchParams,
    limits: rift_search::sparql::GraphLimits,
    coverage: RetrievalCoverage,
    map_error: impl Fn(RiftError) -> E,
    mut read: impl FnMut(SearchParams) -> F,
) -> Result<Result<Box<SearchResult>, SearchUnavailable>, E>
where
    F: std::future::Future<Output = Result<SearchReadResult, E>>,
{
    let one_page = request.max_results.is_some() || request.page != 1;
    if !one_page {
        request.max_results = Some(PAGE_LIMIT_MAX);
    }
    let mut held: Option<Box<SearchResult>> = None;
    let mut bytes_left = limits.literal_bytes_max();
    for _ in 0..PRODUCER_PAGES_MAX {
        let result = read(request.clone()).await?;
        let mut page = match result {
            SearchReadResult::Results(page) => page,
            SearchReadResult::Unavailable(reason) => return Ok(Err(reason)),
        };
        let failure = if page.pagination.page != request.page
            || held.as_ref().is_some_and(|held| {
                page.view != held.view || page.pagination.pages != held.pagination.pages
            }) {
            Some(SymbolUnavailableReason::ViewContext)
        } else {
            let warnings = page.warnings.iter().filter(|warning| {
                !matches!(coverage, RetrievalCoverage::LexicalFallback)
                    || !matches!(
                        warning,
                        ReadWarning::VectorIndexPreparing { .. }
                            | ReadWarning::VectorRankingUnavailable { .. }
                    )
            });
            retrieval_coverage_for(
                warnings,
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
            .page
            .checked_add(1)
            .ok_or_else(|| map_error(coverage_failure()))?;
        let more = !one_page && next <= page.pagination.pages;
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
        request.page = next;
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

#[cfg(test)]
fn retrieval_coverage(warnings: &[ReadWarning], current: bool) -> Option<SymbolUnavailableReason> {
    retrieval_coverage_for(warnings.iter(), current)
}

fn retrieval_coverage_for<'warning>(
    warnings: impl Iterator<Item = &'warning ReadWarning>,
    current: bool,
) -> Option<SymbolUnavailableReason> {
    let mut preparing = false;
    let mut incomplete = false;
    for warning in warnings {
        if failed_retrieval(warning) {
            return Some(SymbolUnavailableReason::ReadFailed);
        }
        if !incomplete_retrieval(warning) {
            continue;
        }
        if current
            && matches!(
                warning,
                ReadWarning::LocalIndexPreparing { .. } | ReadWarning::PatternIndexPreparing { .. }
            )
        {
            preparing = true;
        } else {
            incomplete = true;
        }
    }
    if incomplete {
        Some(SymbolUnavailableReason::InsufficientCoverage)
    } else {
        preparing.then_some(SymbolUnavailableReason::IndexPreparing)
    }
}

/// Whether `warning` names a global API read that failed: the answer lacks the package
/// hits because the request failed, not because the index lacks them.
fn failed_retrieval(warning: &ReadWarning) -> bool {
    matches!(
        warning,
        ReadWarning::GlobalApiUnavailable { .. }
            | ReadWarning::GlobalPublicationIncompatible { .. }
            | ReadWarning::GlobalResponseInvalid { .. }
    )
}

/// Whether `warning` leaves the selected producer pages short of the facts they select.
///
/// A `package_substituted` entry is answered completely from the nearest collected release,
/// so it leaves nothing out.
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
            | ReadWarning::GlobalPageWarning { .. }
            | ReadWarning::PackageAbsent { .. }
            | ReadWarning::PackageRequirementAbsent { .. }
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

    fn substituted() -> ReadWarning {
        ReadWarning::PackageSubstituted {
            entry: rift_protocol::dependencies::PackageContextEntry::new(
                "cargo",
                "tokio",
                rift_protocol::dependencies::PackageSelector::Version("1.47.1".to_owned()),
                rift_protocol::dependencies::PackageAvailability::Canonical,
            ),
            package: rift_protocol::read::PackageIdentity {
                manager: "cargo".to_owned(),
                registry: "crates.io".to_owned(),
                name: "tokio".to_owned(),
                version: "1.51.3".to_owned(),
            },
        }
    }

    #[test]
    fn a_substituted_release_answers_completely() {
        assert_eq!(retrieval_coverage(&[substituted()], true), None);
        assert_eq!(
            retrieval_coverage(
                &[
                    substituted(),
                    ReadWarning::PackageAbsent {
                        package: rift_protocol::read::PackageIdentity {
                            manager: "cargo".to_owned(),
                            registry: "crates.io".to_owned(),
                            name: "missing-helper".to_owned(),
                            version: "0.1.0".to_owned(),
                        },
                    },
                ],
                true,
            ),
            Some(SymbolUnavailableReason::InsufficientCoverage),
            "an absent package still leaves its hits out"
        );
    }

    #[test]
    fn a_failed_global_read_is_not_missing_coverage() {
        use rift_protocol::read::GlobalFailureClass;
        for failed in [
            ReadWarning::GlobalApiUnavailable {
                failure_class: GlobalFailureClass::RetryExhausted,
            },
            ReadWarning::GlobalResponseInvalid {
                failure_class: GlobalFailureClass::InvalidResponse,
            },
            ReadWarning::GlobalPublicationIncompatible {
                failure_class: GlobalFailureClass::CorpusRevision,
            },
        ] {
            assert_eq!(
                retrieval_coverage(
                    &[
                        ReadWarning::LexicalRankingTruncated { matches_max: 1 },
                        failed.clone()
                    ],
                    true,
                ),
                Some(SymbolUnavailableReason::ReadFailed),
                "{failed:?}"
            );
        }
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
