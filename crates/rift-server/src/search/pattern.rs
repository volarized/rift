//! `search` over a regex `pattern`: the files the trigram index selects are verified by the
//! pattern's matcher over the text the published index holds. Every match becomes one file
//! hit, and every declaration holding a match one symbol hit at its first match.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use rift_core::ProjectPath;
use rift_core::line::{line_of, line_starts, without_ending};
use rift_error::errors;
use rift_index::{
    PatternCandidate, PatternCandidates, SymbolMatch, TextSourceFile, UnindexedRows, WorkspaceIndex,
};
use rift_protocol::configuration::SearchConfiguration;
use rift_protocol::read::{
    MatchedField, ReadWarning, ResultOrder, SEARCH_PATTERN_CHARS_MAX, SOURCE_WARNINGS_MAX,
    SearchHit, SearchHitTarget, SearchParams, SearchParamsTarget, SearchResult, SearchScope,
    TextRange,
};
use rift_ranking::{IdentifierMatchClass, Pattern};

use super::{
    HitPayloads, SelectedPaths, StoreAnswer, bound_hits, build_symbol_hit, includes, order_hits,
    populate_symbol_lines, text_file_hit_target,
};
use crate::read::{
    ReadService, RiftError, file_id, page, project_path, results_truncation_warning,
};

/// The `[search]` bounds one `pattern` search runs under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[expect(
    clippy::struct_field_names,
    reason = "each field is one bound, named for the quantity it caps"
)]
pub struct PatternBounds {
    compiled_bytes_max: usize,
    candidate_rows_max: u32,
    verified_bytes_max: u64,
    matches_per_file_max: usize,
}

impl PatternBounds {
    /// Rows of the trigram index one pattern's candidate selection reads at most.
    #[must_use]
    pub const fn candidate_rows_max(self) -> u32 {
        self.candidate_rows_max
    }
}

impl From<&SearchConfiguration> for PatternBounds {
    /// The accepted `[search]` keys. Acceptance bounds each to its advertised range, so the
    /// narrowing conversions here only guard the platform's integer widths.
    fn from(search: &SearchConfiguration) -> Self {
        Self {
            compiled_bytes_max: usize::try_from(search.pattern_compiled_size.bytes())
                .unwrap_or(usize::MAX),
            candidate_rows_max: u32::try_from(search.pattern_candidate_rows).unwrap_or(u32::MAX),
            verified_bytes_max: search.pattern_verified_size.bytes(),
            matches_per_file_max: usize::try_from(search.pattern_matches_per_file)
                .unwrap_or(usize::MAX),
        }
    }
}

impl Default for PatternBounds {
    fn default() -> Self {
        Self::from(&SearchConfiguration::default())
    }
}

/// Why a `pattern` cannot stand beside a field the request also names: the fields that
/// select another result set, and the ones naming a tree the trigram index does not hold.
/// `None` for a request naming no `pattern`.
pub(super) fn pattern_conflict(params: &SearchParams) -> Option<RiftError> {
    params.pattern.as_ref()?;
    let conflicts = [
        (
            params.query.is_some(),
            "`pattern` and `query` select different result sets",
        ),
        (
            params.traversal.is_some(),
            "a walk answers relationships from one seed, and `pattern` answers text",
        ),
        (
            params.change.is_some(),
            "a comparison names committed revisions the trigram index does not hold",
        ),
        (
            params.rev.is_some(),
            "the trigram index holds the current tree alone",
        ),
    ];
    conflicts.into_iter().find_map(|(present, reason)| {
        present.then(|| {
            errors::server::read_invalid()
                .field("pattern")
                .violation(reason)
                .error()
        })
    })
}

/// The request's `pattern`, parsed and compiled under `bounds`, or `None` when it names
/// none.
///
/// # Errors
///
/// Returns `invalid_request` naming `pattern` for a pattern beside a field that selects
/// another result set or tree, for an empty pattern, one past
/// `SEARCH_PATTERN_CHARS_MAX` characters, the unit the schema's `maxLength` counts, one
/// that does not parse, and one whose matcher passes
/// the `[search]` key `pattern_compiled_size`; and `unsupported` for `target:
/// "documentation"`, since a documentation block carries no file text.
pub fn accepted_pattern(
    params: &SearchParams,
    bounds: PatternBounds,
) -> Result<Option<Pattern>, RiftError> {
    let Some(pattern) = params.pattern.as_deref() else {
        return Ok(None);
    };
    if let Some(conflict) = pattern_conflict(params) {
        return conflict.fail();
    }
    if params.target == SearchParamsTarget::Documentation {
        return errors::server::read_unsupported()
            .capability("pattern with target documentation")
            .fail();
    }
    match pattern.chars().count() {
        0 => {
            return errors::server::read_invalid()
                .field("pattern")
                .violation("empty")
                .fail();
        }
        characters if characters > SEARCH_PATTERN_CHARS_MAX => {
            return errors::server::read_invalid()
                .field("pattern")
                .violation(format!(
                    "{characters} characters exceeds the maximum {SEARCH_PATTERN_CHARS_MAX}"
                ))
                .fail();
        }
        _ => {}
    }
    Pattern::parse(pattern, bounds.compiled_bytes_max).map(Some)
}

/// One file to verify: the index holding it, and the spans of it the trigram index
/// selected, or `None` for the whole file.
struct Candidate<'a> {
    owner: &'a WorkspaceIndex,
    file: &'a TextSourceFile,
    spans: Option<Vec<Range<usize>>>,
}

impl<'a> Candidate<'a> {
    /// `file` of `owner`, verified whole.
    const fn whole(owner: &'a WorkspaceIndex, file: &'a TextSourceFile) -> Self {
        Self {
            owner,
            file,
            spans: None,
        }
    }

    /// The spans to verify: the selected rows when every one of them sits on character
    /// boundaries inside the held text, the whole text otherwise. A stored row that
    /// disagrees with the held text is verified whole rather than trusted.
    fn spans(&self) -> Vec<Range<usize>> {
        let text = self.file.content();
        let whole = || std::iter::once(0..text.len()).collect();
        let Some(spans) = self.spans.as_ref() else {
            return whole();
        };
        let fits = |span: &Range<usize>| {
            span.start <= span.end
                && span.end <= text.len()
                && text.is_char_boundary(span.start)
                && text.is_char_boundary(span.end)
        };
        if spans.is_empty() || !spans.iter().all(fits) {
            return whole();
        }
        spans.clone()
    }

    fn path(&self) -> &'a ProjectPath {
        self.file.path()
    }
}

/// A stored row's span as offsets into the held text.
fn text_span(span: &Range<u64>) -> Range<usize> {
    let offset = |value: u64| usize::try_from(value).unwrap_or(usize::MAX);
    offset(span.start)..offset(span.end)
}

/// What one verification pass gathered: the hits, the bytes it read, and the files it cut
/// at their match bound.
struct Verification {
    target: SearchParamsTarget,
    payloads: HitPayloads,
    bounds: PatternBounds,
    results: Vec<SearchHit>,
    verified_bytes: u64,
    cut: Vec<ProjectPath>,
}

impl Verification {
    fn new(target: SearchParamsTarget, payloads: HitPayloads, bounds: PatternBounds) -> Self {
        Self {
            target,
            payloads,
            bounds,
            results: Vec::new(),
            verified_bytes: 0,
            cut: Vec::new(),
        }
    }

    /// Verifies one candidate's spans, refusing once the pass has read past the
    /// `[search]` key `pattern_verified_size`.
    fn verify(&mut self, candidate: &Candidate<'_>, pattern: &Pattern) -> Result<(), RiftError> {
        let spans = candidate.spans();
        let bytes: usize = spans.iter().map(ExactSizeIterator::len).sum();
        self.verified_bytes = self
            .verified_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        if self.verified_bytes > self.bounds.verified_bytes_max {
            return errors::server::read_invalid()
                .field("pattern")
                .violation(format!(
                    "more than {} bytes to verify, the [search] pattern_verified_size bound; \
                     narrow `pattern` or `paths`",
                    self.bounds.verified_bytes_max
                ))
                .fail();
        }
        let text = candidate.file.content();
        let found = spans
            .into_iter()
            .flat_map(|span| pattern.matches(text, span));
        let mut file = FileMatches::new(candidate);
        for (position, matched) in found.enumerate() {
            if position == self.bounds.matches_per_file_max {
                self.cut.push(candidate.path().clone());
                break;
            }
            self.record(&mut file, &matched)?;
        }
        Ok(())
    }

    /// Records one match: a symbol hit the first time a declaration holds a match, and a
    /// file hit for the match itself.
    fn record(
        &mut self,
        file: &mut FileMatches<'_>,
        matched: &Range<usize>,
    ) -> Result<(), RiftError> {
        let start = u64::try_from(matched.start).unwrap_or(u64::MAX);
        let end = u64::try_from(matched.end).unwrap_or(u64::MAX);
        if self.target != SearchParamsTarget::File
            && let Some(hit) = file.declaration_hit(start, end, self.payloads)?
        {
            self.results.push(hit);
        }
        if self.target != SearchParamsTarget::Symbol {
            self.results.push(file.match_hit(start..end, self.payloads));
        }
        Ok(())
    }
}

/// One candidate's matches as they are recorded: the declarations already answered, and
/// the file's line starts once a file hit needs them.
struct FileMatches<'a> {
    candidate: &'a Candidate<'a>,
    declared: BTreeSet<(u64, u64)>,
    starts: Option<Vec<usize>>,
}

impl<'a> FileMatches<'a> {
    fn new(candidate: &'a Candidate<'a>) -> Self {
        Self {
            candidate,
            declared: BTreeSet::new(),
            starts: None,
        }
    }

    /// The hit for the smallest declaration holding `start..end`, when the file is parsed
    /// and that declaration has not answered yet.
    fn declaration_hit(
        &mut self,
        start: u64,
        end: u64,
        payloads: HitPayloads,
    ) -> Result<Option<SearchHit>, RiftError> {
        let owner = self.candidate.owner;
        let Some(parsed) = owner.file(self.candidate.path()) else {
            return Ok(None);
        };
        let Some(symbol) = parsed.enclosing_symbol(start, end) else {
            return Ok(None);
        };
        if !self.declared.insert((symbol.range.start, symbol.range.end)) {
            return Ok(None);
        }
        let matched = SymbolMatch {
            file: parsed,
            symbol,
            // A pattern hit carries no score, so the identifier rank no reader weighs is
            // the one class a text match reaches a declaration through.
            rank: IdentifierMatchClass::Substring,
        };
        build_symbol_hit(owner, matched, None, vec![MatchedField::Content], payloads).map(Some)
    }

    /// The file hit for one match: its byte range, its line, and the line itself when
    /// the request asked for `source`.
    fn match_hit(&mut self, matched: Range<u64>, payloads: HitPayloads) -> SearchHit {
        let file = self.candidate.file;
        let text = file.content();
        let starts = self.starts.get_or_insert_with(|| line_starts(text));
        let line = line_of(starts, matched.start);
        let hit = match self.candidate.owner.file(file.path()) {
            Some(parsed) => SearchHitTarget::File {
                size: u64::try_from(text.len()).unwrap_or(u64::MAX),
                languages: vec![parsed.syntax().language().clone()],
            },
            None => text_file_hit_target(file),
        };
        SearchHit {
            hit,
            score: None,
            matched_by: vec![MatchedField::Content],
            source: payloads
                .source
                .then(|| matched_line(text, starts, line).to_owned()),
            range: Some(TextRange {
                start: matched.start,
                end: matched.end,
            }),
            line: Some(line),
            path: Some(project_path(file.path())),
            unit: None,
            traversal_path: None,
            distance: None,
            change: None,
        }
    }
}

/// The text of 1-based `line`, without its ending.
fn matched_line<'t>(text: &'t str, starts: &[usize], line: u64) -> &'t str {
    let index = usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX);
    let start = starts.get(index).copied().unwrap_or(text.len());
    let end = starts.get(index + 1).copied().unwrap_or(text.len());
    without_ending(&text[start..end])
}

/// The one warning naming the files cut at their match bound, at most
/// `SOURCE_WARNINGS_MAX` of them, or none when no file was cut.
fn matches_truncation_warning(cut: &[ProjectPath], bounds: PatternBounds) -> Option<ReadWarning> {
    if cut.is_empty() {
        return None;
    }
    Some(ReadWarning::PatternMatchesTruncated {
        matches_per_file: u64::try_from(bounds.matches_per_file_max).unwrap_or(u64::MAX),
        files: cut.iter().take(SOURCE_WARNINGS_MAX).map(file_id).collect(),
    })
}

impl ReadService {
    /// Answers the project side of a `pattern` search.
    ///
    /// The store's candidates name the files and rows to verify; a notebook and a file
    /// holding a line longer than one chunk join them whole, and so does every
    /// `paths.force_include` file. A pattern with no prefilter, or a store that did not
    /// answer, verifies every held file whole under the same bounds. The hits carry no
    /// score, so `relevance` keeps them in path order, then by offset, a declaration at
    /// its first match. A `global` scope verifies no project file: its matches come from
    /// the packages alone, which the caller adds.
    ///
    /// Rows the trigram index lacks are verified beside the ones it selected while both
    /// fit the `[search]` bounds, so the answer stays complete; past them the answer
    /// covers the rows the index holds and warns `pattern_index_preparing`.
    pub(super) fn search_pattern(
        &self,
        params: &SearchParams,
        pattern: &Pattern,
        store: &StoreAnswer,
    ) -> Result<SearchResult, RiftError> {
        self.validate_dependency_scope(params.scope, params.rev.as_ref())?;
        let bounds = store.pattern_bounds();
        let limit = super::search_page_limit(params)?;
        let selected = self.selected_paths(params.paths.as_ref())?;
        let (candidates, preparing) = if params.scope == SearchScope::Global {
            (Vec::new(), None)
        } else {
            self.pattern_candidates(pattern, store.pattern_candidates(), &selected, bounds)?
        };
        let results_max = self.index().results_max();
        let mut verification =
            Verification::new(params.target, HitPayloads::requested(params), bounds);
        for candidate in &candidates {
            if verification.results.len() >= results_max {
                break;
            }
            verification.verify(candidate, pattern)?;
        }
        let mut results = verification.results;
        let mut warnings = self.warnings();
        warnings.extend(selected.warnings());
        warnings.extend(matches_truncation_warning(&verification.cut, bounds));
        warnings.extend(preparing);
        if params.order != ResultOrder::Relevance {
            order_hits(&mut results, params.order);
        }
        if let Some(bound) = bound_hits(&mut results, results_max) {
            warnings.push(results_truncation_warning(bound));
        }
        let (mut results, pagination) = page(results, params.page_index, limit);
        populate_symbol_lines(&mut results, self.index(), selected.force_include.as_ref())?;
        Ok(SearchResult {
            results,
            pagination,
            warnings,
        })
    }

    /// The files to verify, in path order, and the warning the answer carries when rows
    /// the trigram index lacks were left out.
    ///
    /// The store's selection stands when the pattern has a prefilter and the store
    /// answered, merged with the rows the index lacks when the store listed them and the
    /// merged set fits `pattern_verified_size`; every searched file otherwise. Each set is
    /// screened by the request's `paths`, and every `paths.force_include` file joins whole.
    fn pattern_candidates<'a>(
        &'a self,
        pattern: &Pattern,
        selection: Option<&'a PatternCandidates>,
        selected: &'a SelectedPaths,
        bounds: PatternBounds,
    ) -> Result<(Vec<Candidate<'a>>, Option<ReadWarning>), RiftError> {
        let index = self.index();
        let Some(selection) = selection.filter(|_| pattern.prefilter().is_some()) else {
            let every = index
                .searched_text_files()
                .map(|file| Candidate::whole(index, file))
                .collect();
            return Ok((self.screened(every, selected), None));
        };
        if let Some(rows_max) = selection.truncated_at() {
            return errors::server::read_invalid()
                .field("pattern")
                .violation(format!(
                    "more than {rows_max} candidate rows, the [search] pattern_candidate_rows \
                     bound; narrow `pattern` or `paths`"
                ))
                .fail();
        }
        let held = self.screened(self.selected_candidates(selection.candidates()), selected);
        let Some(unindexed) = selection.unindexed() else {
            return Ok((held, None));
        };
        if let Some(completed) = unindexed.completed() {
            let complete = self.screened(self.selected_candidates(completed), selected);
            if planned_bytes(&complete) <= bounds.verified_bytes_max {
                return Ok((complete, None));
            }
        }
        Ok((held, Some(pattern_index_preparing(unindexed))))
    }

    /// `chosen`, screened by the request's `paths`, with every `paths.force_include` file
    /// whole, in path order. A file named twice is verified as its later entry names it.
    fn screened<'a>(
        &'a self,
        chosen: Vec<Candidate<'a>>,
        selected: &'a SelectedPaths,
    ) -> Vec<Candidate<'a>> {
        let index = self.index();
        let mut files: BTreeMap<&'a ProjectPath, Candidate<'a>> = chosen
            .into_iter()
            .filter(|candidate| includes(selected.matcher.as_ref(), index.root(), candidate.path()))
            .map(|candidate| (candidate.path(), candidate))
            .collect();
        if let Some(extra) = selected.force_include.as_ref() {
            files.extend(
                extra
                    .text_files()
                    .map(|file| (file.path(), Candidate::whole(extra, file))),
            );
        }
        files.into_values().collect()
    }

    /// `rows` as the files to verify, and every file the trigram index cannot rule out,
    /// whole: its whole-file candidates follow the selected rows, so a file both name is
    /// verified whole.
    fn selected_candidates<'a>(&'a self, rows: &'a [PatternCandidate]) -> Vec<Candidate<'a>> {
        let index = self.index();
        let rows = rows.iter().filter_map(|chosen| {
            let file = index.text_file(chosen.path())?;
            let spans = chosen.spans().iter().map(text_span).collect();
            Some(Candidate {
                owner: index,
                file,
                spans: Some(spans),
            })
        });
        let whole = index
            .whole_file_candidates()
            .map(|file| Candidate::whole(index, file));
        rows.chain(whole).collect()
    }
}

/// The bytes of text verifying every one of `candidates` reads.
fn planned_bytes(candidates: &[Candidate<'_>]) -> u64 {
    let bytes: usize = candidates
        .iter()
        .flat_map(Candidate::spans)
        .map(|span| span.len())
        .sum();
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// The warning an answer carries when it covers the rows the trigram index holds alone.
fn pattern_index_preparing(unindexed: &UnindexedRows) -> ReadWarning {
    let prepared = unindexed.prepared();
    let total = unindexed.total();
    ReadWarning::PatternIndexPreparing {
        prepared,
        total,
        detail: format!(
            "{prepared} of {total} rows of file text are in the trigram index, so a match in \
             the other rows is missing from this answer; resend the request once the trigram \
             index has caught up"
        ),
    }
}
