//! Retains the selected producer pages, up to the retrieval bounds, before graph evaluation.

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

/// Acquires the producer pages one query service selects, up to the retrieval's bounds.
///
/// A producer bound never refuses the retrieval: a page that reports a producer bound
/// (`results_truncated`, `lexical_ranking_truncated`, `pattern_matches_truncated`,
/// `query_narrowed`, `change_truncated`) is held as the producer answered it, and the
/// acquisition stops at `solutions_max` rows, at `literal_bytes_max` bytes, or after
/// [`PRODUCER_PAGES_MAX`] pages with what it holds. The evaluation then answers and pages
/// over those rows, so `pagination.total` counts what the producer returned. Each cut is
/// written to the log once per retrieval and never reaches the answer. Missing facts (a
/// preparing index, a stale view, an absent package, a failed global read) still refuse.
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
    let mut cuts = ProducerCuts::default();
    let mut bytes_left = limits.literal_bytes_max();
    for _ in 0..PRODUCER_PAGES_MAX {
        let result = read(request.clone()).await?;
        let mut page = match result {
            SearchReadResult::Results(page) => page,
            SearchReadResult::Unavailable(reason) => return Ok(Err(reason)),
        };
        cuts.take_reported(&mut page.warnings);
        if let Some(reason) = page_failure(&request, &page, held.as_deref(), coverage) {
            return Ok(Err(unavailable(reason, page)));
        }
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
        let room = limits
            .solutions_max()
            .saturating_sub(held.as_ref().map_or(0, |held| held.results.len()));
        if page.results.len() > room {
            page.results.truncate(room);
            cuts.note(ProducerCut::Solutions {
                max: limits.solutions_max(),
            });
        }
        let fitting = fitting_results(&page, bytes_left).map_err(&map_error)?;
        if fitting < page.results.len() {
            page.results.truncate(fitting);
            cuts.note(ProducerCut::Bytes {
                max: limits.literal_bytes_max(),
            });
        }
        let bytes = crate::proxy::encode_bounded(&page, bytes_left)
            .map_err(|_| map_error(coverage_failure()))?;
        bytes_left -= bytes.len();
        let stopped = cuts.stops_acquisition();
        hold_page(&mut held, &mut request, page);
        if !more || stopped {
            cuts.record(&request);
            return held.map(Ok).ok_or_else(|| map_error(coverage_failure()));
        }
        request.page = next;
    }
    cuts.note(ProducerCut::Pages {
        max: PRODUCER_PAGES_MAX,
    });
    cuts.record(&request);
    held.map(Ok).ok_or_else(|| map_error(coverage_failure()))
}

/// Why one acquired page cannot join the held ones: it answers another page or another
/// view, or a warning says the facts it selects are missing.
fn page_failure(
    request: &SearchParams,
    page: &SearchResult,
    held: Option<&SearchResult>,
    coverage: RetrievalCoverage,
) -> Option<SymbolUnavailableReason> {
    let moved = page.pagination.page != request.page
        || held.is_some_and(|held| {
            page.view != held.view || page.pagination.pages != held.pagination.pages
        });
    if moved {
        return Some(SymbolUnavailableReason::ViewContext);
    }
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
}

/// Adds `page` to the held rows, pinning later pages to the view the first one names.
fn hold_page(
    held: &mut Option<Box<SearchResult>>,
    request: &mut SearchParams,
    mut page: Box<SearchResult>,
) {
    if let Some(held) = held {
        held.results.append(&mut page.results);
        for warning in page.warnings {
            if !held.warnings.contains(&warning) {
                held.warnings.push(warning);
            }
        }
    } else {
        request.view = page.view.as_ref().map(|view| view.id.clone());
        *held = Some(page);
    }
}

/// Cuts one producer's rows to the room `held_rows` and `bytes_left` leave under the graph
/// bounds, for a caller combining several producers' answers into one, and writes any cut
/// to the log. Answers whether a bound cut the rows, so the caller reads no further producer.
///
/// # Errors
///
/// Returns the graph coverage failure when not even an empty page fits `bytes_left`.
pub(crate) fn bound_combined_rows(
    result: &mut SearchResult,
    held_rows: usize,
    bytes_left: usize,
    limits: rift_search::sparql::GraphLimits,
    request: &SearchParams,
) -> Result<bool, RiftError> {
    let mut cuts = ProducerCuts::default();
    let room = limits.solutions_max().saturating_sub(held_rows);
    if result.results.len() > room {
        result.results.truncate(room);
        cuts.note(ProducerCut::Solutions {
            max: limits.solutions_max(),
        });
    }
    let fitting = fitting_results(result, bytes_left)?;
    if fitting < result.results.len() {
        result.results.truncate(fitting);
        cuts.note(ProducerCut::Bytes {
            max: limits.literal_bytes_max(),
        });
    }
    cuts.record(request);
    Ok(cuts.stops_acquisition())
}

/// How many of `page`'s leading rows encode within `bytes_max`.
///
/// Encodes at most `log2(page.results.len()) + 2` times: a full page that fits answers
/// at once, and otherwise a binary search over the row count finds the longest prefix.
fn fitting_results(page: &SearchResult, bytes_max: usize) -> Result<usize, RiftError> {
    let fits = |count: usize| {
        let mut prefix = page.clone();
        prefix.results.truncate(count);
        crate::proxy::encode_bounded(&prefix, bytes_max).is_ok()
    };
    let rows = page.results.len();
    if fits(rows) {
        return Ok(rows);
    }
    if !fits(0) {
        return Err(coverage_failure());
    }
    let (mut fitting, mut over) = (0, rows);
    while over - fitting > 1 {
        let middle = fitting + (over - fitting) / 2;
        if fits(middle) {
            fitting = middle;
        } else {
            over = middle;
        }
    }
    Ok(fitting)
}

/// Distinct producer cuts one retrieval records, at most this many.
const PRODUCER_CUTS_MAX: usize = 8;

/// A bound that stopped one retrieval short of everything its producer could select.
#[derive(Clone, Debug, PartialEq)]
enum ProducerCut {
    /// The producer reported that it stopped at one of its own bounds.
    Reported(Box<ReadWarning>),
    /// The retrieval held `max` rows, the graph's solution bound, and read no further page.
    Solutions { max: usize },
    /// The held rows reached `max` encoded bytes, the graph's literal byte bound.
    Bytes { max: usize },
    /// The retrieval read `max` producer pages, [`PRODUCER_PAGES_MAX`].
    Pages { max: usize },
}

/// The cuts one retrieval met, each once, written to the log when it ends.
#[derive(Debug, Default)]
struct ProducerCuts {
    cuts: Vec<ProducerCut>,
    stopped: bool,
}

impl ProducerCuts {
    /// Moves the producer's bound reports out of `warnings`: the rows stand as the producer
    /// answered them, and the answer carries nothing for the cut.
    fn take_reported(&mut self, warnings: &mut Vec<ReadWarning>) {
        let (reported, kept): (Vec<_>, Vec<_>) = std::mem::take(warnings)
            .into_iter()
            .partition(is_producer_bound);
        *warnings = kept;
        for warning in reported {
            self.note(ProducerCut::Reported(Box::new(warning)));
        }
    }

    fn note(&mut self, cut: ProducerCut) {
        self.stopped |= !matches!(cut, ProducerCut::Reported(_));
        if self.cuts.len() < PRODUCER_CUTS_MAX && !self.cuts.contains(&cut) {
            self.cuts.push(cut);
        }
    }

    /// Whether a retrieval bound, rather than the producer, ended the acquisition.
    const fn stops_acquisition(&self) -> bool {
        self.stopped
    }

    /// Writes one warning record per distinct cut, at most [`PRODUCER_CUTS_MAX`].
    fn record(&self, request: &SearchParams) {
        for cut in &self.cuts {
            let (bound, value) = match cut {
                ProducerCut::Reported(warning) => (
                    "producer",
                    serde_json::to_string(warning.as_ref()).unwrap_or_default(),
                ),
                ProducerCut::Solutions { max } => ("solutions_max", max.to_string()),
                ProducerCut::Bytes { max } => ("literal_bytes_max", max.to_string()),
                ProducerCut::Pages { max } => ("producer_pages_max", max.to_string()),
            };
            let target = &request.target;
            rift_tracing::warn!(
                component = "search",
                operation = "search.retrieval",
                bound = bound,
                value = %value,
                target = ?target,
                "a producer bound ended the retrieval; the answer holds the rows it returned"
            );
        }
    }
}

/// Whether `warning` reports that a producer stopped at one of its own bounds while every
/// row it returned stands.
fn is_producer_bound(warning: &ReadWarning) -> bool {
    matches!(
        warning,
        ReadWarning::ResultsTruncated { .. }
            | ReadWarning::LexicalRankingTruncated { .. }
            | ReadWarning::PatternMatchesTruncated { .. }
            | ReadWarning::QueryNarrowed { .. }
            | ReadWarning::ChangeTruncated { .. }
    )
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

pub(crate) fn retrieval_coverage_for<'warning>(
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
            | ReadWarning::PatternIndexPreparing { .. }
            | ReadWarning::HistoryStoreFilling { .. }
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
        // D37: a producer bound is no missing fact, so preparation beside it stays
        // retryable, and preparation beside a missing fact still refuses.
        assert_eq!(
            retrieval_coverage(
                &[
                    preparing.clone(),
                    ReadWarning::LexicalRankingTruncated { matches_max: 1 }
                ],
                true,
            ),
            Some(SymbolUnavailableReason::IndexPreparing)
        );
        assert_eq!(
            retrieval_coverage(&[preparing, ReadWarning::GlobalAccessDisabled], true),
            Some(SymbolUnavailableReason::InsufficientCoverage)
        );
    }

    /// D37 (#713): every producer bound leaves the retrieval answering what it returned.
    #[test]
    fn producer_bound_does_not_refuse_search() {
        for bound in [
            ReadWarning::ResultsTruncated { results_max: 1000 },
            ReadWarning::LexicalRankingTruncated { matches_max: 1000 },
            ReadWarning::QueryNarrowed { terms_max: 4 },
        ] {
            assert!(super::is_producer_bound(&bound), "{bound:?}");
            assert!(!incomplete_retrieval(&bound), "{bound:?}");
            assert_eq!(
                retrieval_coverage(std::slice::from_ref(&bound), true),
                None,
                "{bound:?}"
            );
            assert_eq!(
                retrieval_coverage(std::slice::from_ref(&bound), false),
                None,
                "{bound:?}"
            );
        }
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

    /// The authored search example, `rows` hits long, as page `page` of `pages`.
    fn page_of(rows: usize, page: u64, pages: u64) -> rift_protocol::read::SearchResult {
        let schema = schemars::schema_for!(rift_protocol::read::SearchResult);
        let example = schema
            .get("examples")
            .and_then(|examples| examples.get(0))
            .cloned()
            .expect("the model states an example");
        let mut result: rift_protocol::read::SearchResult =
            serde_json::from_value(example).expect("the example is a search result");
        let hit = result.results[0].clone();
        result.results = vec![hit; rows];
        result.pagination.page = page;
        result.pagination.pages = pages;
        result.warnings = vec![ReadWarning::ResultsTruncated { results_max: 1000 }];
        result.view = Some(
            serde_json::from_value(serde_json::json!({
                "id": "view.fixture",
                "expires_at": "2099-01-01T00:00:00Z"
            }))
            .expect("a captured view"),
        );
        result
    }

    /// Runs one retrieval over `pages` producer pages of `rows` hits each, under a graph
    /// solution bound of `solutions_max`, and returns the held rows and the cut records.
    fn retrieve(
        pages: u64,
        rows: usize,
        solutions_max: usize,
    ) -> (
        Result<Box<rift_protocol::read::SearchResult>, String>,
        Vec<serde_json::Value>,
    ) {
        let limits = rift_search::sparql::GraphLimits::builder()
            .solutions_max(solutions_max)
            .build()
            .expect("graph limits");
        let request: rift_protocol::read::SearchParams =
            serde_json::from_value(serde_json::json!({"query": "load_config"})).expect("a search");
        let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .capture("warn")
            .install()
            .expect("recorder");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let collected = runtime.block_on(super::collect_retrieval(
            request,
            limits,
            |error| error.to_string(),
            |params: rift_protocol::read::SearchParams| async move {
                Ok::<_, String>(super::SearchReadResult::Results(Box::new(page_of(
                    rows,
                    params.page,
                    pages,
                ))))
            },
        ));
        let held = match collected {
            Ok(Ok(held)) => Ok(held),
            Ok(Err(unavailable)) => Err(format!("{unavailable:?}")),
            Err(error) => Err(error),
        };
        let records = drain
            .queued_records()
            .into_iter()
            .filter(|record| {
                record
                    .message()
                    .starts_with("a producer bound ended the retrieval")
            })
            .map(|record| {
                assert_eq!(record.level(), "warn", "{record:?}");
                serde_json::from_str(record.fields()).expect("fields")
            })
            .collect();
        (held, records)
    }

    fn bounds(records: &[serde_json::Value]) -> Vec<&str> {
        records
            .iter()
            .map(|record| record["bound"].as_str().expect("bound"))
            .collect()
    }

    /// D37 (#713): the graph solution bound at its edges. Below and at it every producer row
    /// is held; above it the retrieval holds `solutions_max` rows and stops, and the answer
    /// carries no warning for either cut, which the log records once each.
    #[test]
    fn the_solution_bound_holds_up_to_its_value_and_logs_the_cut() {
        for (pages, rows, solutions_max, held_rows, logged) in [
            (2_u64, 3_usize, 7_usize, 6_usize, vec!["producer"]),
            (2, 3, 6, 6, vec!["producer"]),
            (3, 3, 7, 7, vec!["producer", "solutions_max"]),
            (40, 3, 7, 7, vec!["producer", "solutions_max"]),
        ] {
            let (held, records) = retrieve(pages, rows, solutions_max);
            let held = held.expect("a producer bound answers");
            assert_eq!(held.results.len(), held_rows, "pages={pages}");
            assert!(
                held.warnings
                    .iter()
                    .all(|warning| !super::is_producer_bound(warning)),
                "the answer carries nothing for the cut: {:?}",
                held.warnings
            );
            assert_eq!(bounds(&records), logged, "pages={pages}");
        }
    }

    /// D37 (#713): the page bound ends the acquisition with every row it read, and the log
    /// records it once, whatever the number of pages left unread.
    #[test]
    fn the_page_bound_holds_what_it_read_and_logs_one_record() {
        let pages_max = u64::try_from(super::PRODUCER_PAGES_MAX).expect("fits");
        for (pages, logged) in [
            (pages_max, vec!["producer"]),
            (pages_max + 1, vec!["producer", "producer_pages_max"]),
            (pages_max * 4, vec!["producer", "producer_pages_max"]),
        ] {
            let (held, records) = retrieve(pages, 1, 10_000);
            let held = held.expect("a producer bound answers");
            assert_eq!(
                held.results.len(),
                usize::try_from(pages.min(pages_max)).expect("fits")
            );
            assert_eq!(bounds(&records), logged, "pages={pages}");
        }
    }
}
