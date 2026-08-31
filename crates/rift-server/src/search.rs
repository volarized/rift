//! `search` tool execution: lexical symbol and content-line search over syntax-indexed and
//! `[search.text]` files alike, narrowed or extended by a request's `paths` selector, plus a
//! bounded relationship `traversal` from one seed symbol. Extracted from `read` so that module
//! stays below its size bound.

use std::cmp::Ordering;
use std::collections::{BTreeSet, VecDeque};
use std::path::Path;

use rift_core::ProjectPath;
use rift_core::constants::{FORCE_INCLUDE_FILES_MAX, SEARCH_RESULTS_DEFAULT};
use rift_core::{LoopBudget, SymbolId as CoreSymbolId, line};
use rift_index::{
    IndexedFile, LexicalChange, LexicalUnit, LexicalUnitKind, PathChanges, PathMatcher,
    RelationshipEdge, RelationshipStore, SymbolMatch, SymbolMatchRank, TextSourceFile,
    WorkspaceIndex,
};
use rift_protocol::read::{
    ExactKind, Extensions, GraphHop, HopDirection, MatchedField, PathPattern, PathSelector,
    ReadWarning, Relationship, RelationshipDerivation, ResultOrder, SEARCH_TRAVERSAL_DEPTH_MAX,
    SEARCH_TRAVERSAL_DEPTH_MIN, SEARCH_TRAVERSAL_FACETS_MAX, SearchHit, SearchHitTarget,
    SearchInclude, SearchParams, SearchParamsTarget, SearchResult, SearchTraversal, SymbolId,
    TraversalDirection,
};
use rift_search::{Declaration, DescribedUnit, RankedUnit};
use rift_syntax::{ByteRange, SyntaxSymbol};

use crate::change::parse_symbol_address;
use crate::read::{
    ReadError, ReadFault, ReadService, accepted_limit, excerpt, page, project_path, text_range,
    validate_common, wire_symbol,
};

impl ReadService {
    /// Searches indexed declarations and source lines, optionally narrowed or extended by
    /// `params.paths`, merged with `ranked` from the caller's search index - the lexical
    /// and semantic rankings already fused into one score. `ranked` is empty when that
    /// index is unavailable or its stamped revision no longer matches what is published -
    /// the caller decides that, this method only merges what it is handed.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for an invalid `paths` glob or a `force_include` bound crossed.
    pub fn search(
        &self,
        params: &SearchParams,
        ranked: &[RankedUnit],
    ) -> Result<SearchResult, ReadError> {
        validate_search(params)?;
        if self.revision().is_some() && force_include_requested(params) {
            return Err(ReadFault::unsupported("force_include at a revision"));
        }
        let query = params.query.as_deref();
        if query.is_none() && params.traversal.is_none() {
            return Err(ReadFault::invalid("query", "missing"));
        }
        if query.is_some_and(str::is_empty) {
            return Err(ReadFault::invalid("query", "empty"));
        }
        let limit = accepted_limit(params.limit.unwrap_or(SEARCH_RESULTS_DEFAULT as u64))?;
        let root = self.index().root();
        let selector = params.paths.as_ref();
        let matcher = path_matcher(root, selector)?;
        let payloads = HitPayloads::requested(params);
        // The whole candidate pool is collected up to the index's own `results_max` bound -
        // bounded work whatever the page size - then ordered and truncated to that same
        // bound, so `pagination.total_pages` counts the full result set and every page is
        // one window of the same ordering.
        let fetch_limit = self.index().results_max();

        let mut results = Vec::new();
        if let Some(query) = query {
            let criteria = SearchCriteria {
                query,
                target: params.target,
                payloads,
            };
            collect_indexed_hits(
                self.index(),
                matcher.as_ref(),
                root,
                criteria,
                fetch_limit,
                &mut results,
            )?;
            if results.len() < fetch_limit
                && let Some(selector) = selector
            {
                collect_force_include_hits(
                    self.index(),
                    selector,
                    criteria,
                    fetch_limit,
                    &mut results,
                )?;
            }
            collect_ranked_hits(
                self.index(),
                matcher.as_ref(),
                root,
                criteria,
                ranked,
                &mut results,
            )?;
        }
        let mut traversal_truncated = false;
        if let Some(traversal) = params.traversal.as_ref()
            && matches!(
                params.target,
                SearchParamsTarget::All | SearchParamsTarget::Symbol
            )
        {
            traversal_truncated = collect_traversal_hits(
                self,
                matcher.as_ref(),
                root,
                traversal,
                payloads,
                &mut results,
            )?;
        }
        order_hits(&mut results, params.order);
        results.truncate(fetch_limit);
        let (mut results, pagination) = page(results, params.page_index, limit);
        if !payloads.score {
            for hit in &mut results {
                hit.score = None;
            }
        }
        Ok(SearchResult {
            results,
            pagination,
            warnings: {
                let mut warnings = self.warnings();
                if traversal_truncated {
                    warnings.push(traversal_truncation_warning());
                }
                warnings
            },
        })
    }

    /// Derives the lexical write one change set owes: the paths whose stored units go, and
    /// the units this snapshot derived for the paths it read.
    ///
    /// Every named path is replaced, so a path this snapshot read appears in both halves
    /// and one it found gone appears only in the first. Replacing rather than adding is
    /// what lets the same change set be written twice: two rebuilds captured from one
    /// publication both write what they read, and the second leaves what the first left.
    #[must_use]
    pub fn lexical_change(&self, changes: &PathChanges) -> LexicalChange {
        LexicalChange::new(
            changes.paths().cloned().collect(),
            self.index().lexical_units_for(changes.indexed()),
        )
    }

    /// Pairs each symbol unit in `units` with the declaration the semantic tier embeds for
    /// it.
    ///
    /// Only a symbol unit carries a declaration: a text file's chunk describes none, so it
    /// has no entry and the two slices are never parallel. Each pair is built from one
    /// unit's own resolution, so a unit can never pick up another unit's declaration.
    ///
    /// The declaration's own source is the whole document: [`LexicalUnit::content`] already
    /// holds the bytes the symbol was indexed from, and the text builder prefers source
    /// over any metadata line.
    ///
    /// The walk runs over `units`, whose length this snapshot's own file and symbol bounds
    /// already fixed, and each symbol unit costs one scan of the file it names, which is
    /// how `resolve_symbol` narrows the lookup.
    #[must_use]
    pub fn described_units<'a>(&'a self, units: &'a [LexicalUnit]) -> Vec<DescribedUnit<'a>> {
        units
            .iter()
            .filter(|unit| unit.kind() == LexicalUnitKind::Symbol)
            .filter_map(|unit| self.described_unit(unit))
            .collect()
    }

    /// How many visible files this snapshot indexes across syntax and baseline text.
    ///
    /// A caller estimates the semantic tier's preparation work from this count.
    #[must_use]
    pub fn file_count(&self) -> u64 {
        let files = self.index().files().len() + self.index().text_files().len();
        u64::try_from(files).unwrap_or(u64::MAX)
    }

    /// One symbol unit paired with its declaration, or nothing when this snapshot no longer
    /// holds the symbol the unit names.
    fn described_unit<'a>(&'a self, unit: &'a LexicalUnit) -> Option<DescribedUnit<'a>> {
        let (_, symbol) = resolve_symbol(self.index(), unit.path(), unit.identity())?;
        let declaration =
            Declaration::new(symbol.kind, &symbol.qualified_name).source(unit.content());
        Some(DescribedUnit::new(unit, declaration))
    }
}

/// Whether the request's `paths` selector names any `force_include` glob.
/// Reaching index-excluded files is a walk of the working tree, which a
/// revision-addressed search has no tree to run against.
fn force_include_requested(params: &SearchParams) -> bool {
    params
        .paths
        .as_ref()
        .is_some_and(|selector| !selector.force_include.is_empty())
}

/// Which extra payload `params.include` asked to attach to every hit, derived once per
/// request: `source` attaches the excerpt, `score` attaches the fused ranking value.
#[derive(Clone, Copy, Debug, Default)]
struct HitPayloads {
    source: bool,
    score: bool,
}

impl HitPayloads {
    fn requested(params: &SearchParams) -> Self {
        let include = params.include.as_deref().unwrap_or_default();
        Self {
            source: include.contains(&SearchInclude::Source),
            score: include.contains(&SearchInclude::Score),
        }
    }
}

/// Query term, kind selector, and requested payloads shared by every hit-collection pass
/// one `search` call runs: identifier matching, `force_include`, and the merge of ranked
/// units.
#[derive(Clone, Copy, Debug)]
struct SearchCriteria<'a> {
    query: &'a str,
    target: SearchParamsTarget,
    payloads: HitPayloads,
}

fn validate_search(params: &SearchParams) -> Result<(), ReadError> {
    validate_common(params.rev.is_some())?;
    if let Some(selector) = params.paths.as_ref() {
        validate_path_selector(selector)?;
    }
    if let Some(traversal) = params.traversal.as_ref() {
        validate_traversal(traversal)?;
        if params.rev.is_some() {
            return Err(ReadFault::unsupported("traversal at a revision"));
        }
    }
    Ok(())
}

/// Refuses `traversal` when `depth` or `facets` breaks the bound its schema advertises.
/// `schemars`' `range`/`length` constraints are advisory only, so this mirrors them the same
/// way [`validate_path_selector`] mirrors [`PathPattern`]'s.
fn validate_traversal(traversal: &SearchTraversal) -> Result<(), ReadError> {
    if !(SEARCH_TRAVERSAL_DEPTH_MIN..=SEARCH_TRAVERSAL_DEPTH_MAX).contains(&traversal.depth) {
        return Err(ReadFault::invalid(
            "traversal",
            format!(
                "depth {} outside {SEARCH_TRAVERSAL_DEPTH_MIN} to {SEARCH_TRAVERSAL_DEPTH_MAX}",
                traversal.depth
            ),
        ));
    }
    if traversal.facets.len() > SEARCH_TRAVERSAL_FACETS_MAX {
        return Err(ReadFault::invalid(
            "traversal",
            format!(
                "facets carries {} entries, more than {SEARCH_TRAVERSAL_FACETS_MAX}",
                traversal.facets.len()
            ),
        ));
    }
    Ok(())
}

/// Refuses `selector` when any `include`, `exclude`, or `force_include` pattern breaks
/// [`PathPattern`]'s forward-slash-only contract, before it reaches a glob engine that would
/// otherwise read a stray backslash as an escape.
fn validate_path_selector(selector: &PathSelector) -> Result<(), ReadError> {
    let patterns = selector
        .include
        .iter()
        .chain(&selector.exclude)
        .chain(&selector.force_include);
    for pattern in patterns {
        if let Some(violation) = pattern.violation() {
            return Err(ReadFault::invalid("paths", violation.as_str()));
        }
    }
    Ok(())
}

/// Compiles `selector`'s `include`/`exclude` globs into one matcher, or none when neither list
/// is set. `force_include` is handled separately by [`collect_force_include_hits`]: it reaches
/// files the index never held, so it never narrows the indexed candidate set this matcher
/// screens.
fn path_matcher(
    root: &Path,
    selector: Option<&PathSelector>,
) -> Result<Option<PathMatcher>, ReadError> {
    let Some(selector) = selector else {
        return Ok(None);
    };
    if selector.include.is_empty() && selector.exclude.is_empty() {
        return Ok(None);
    }
    PathMatcher::build(
        root,
        &pattern_strings(&selector.include),
        &pattern_strings(&selector.exclude),
    )
    .map(Some)
    .map_err(ReadFault::index)
}

fn pattern_strings(patterns: &[PathPattern]) -> Vec<String> {
    patterns.iter().map(|pattern| pattern.0.clone()).collect()
}

/// Whether `path` (project-relative) passes `matcher`, absent a matcher including every path.
fn includes(matcher: Option<&PathMatcher>, root: &Path, path: &ProjectPath) -> bool {
    matcher.is_none_or(|matcher| matcher.includes(&root.join(path.as_str())))
}

/// Symbol and content-line hits from the index, filtered by `matcher` and collected up to
/// `fetch_limit` - the index's `results_max` bound, never the smaller page size - because
/// [`order_hits`] orders this whole pool before one page is cut out of it: stopping
/// collection at the page size could drop a later, higher-scoring candidate, and the page
/// count states the full result set.
fn collect_indexed_hits(
    index: &WorkspaceIndex,
    matcher: Option<&PathMatcher>,
    root: &Path,
    criteria: SearchCriteria<'_>,
    fetch_limit: usize,
    results: &mut Vec<SearchHit>,
) -> Result<(), ReadError> {
    let SearchCriteria {
        query,
        target,
        payloads,
    } = criteria;
    if matches!(target, SearchParamsTarget::All | SearchParamsTarget::Symbol) {
        for matched in index
            .symbols(query, fetch_limit)
            .map_err(ReadFault::index)?
        {
            if !includes(matcher, root, matched.file.path()) {
                continue;
            }
            results.push(symbol_search_hit(index, matched, payloads)?);
            if results.len() >= fetch_limit {
                return Ok(());
            }
        }
    }
    if results.len() < fetch_limit
        && matches!(target, SearchParamsTarget::All | SearchParamsTarget::File)
    {
        collect_indexed_content_hits(index, matcher, root, query, fetch_limit, payloads, results)?;
    }
    Ok(())
}

/// Content-line hits from baseline catalog, filtered by `matcher` and appended to
/// `results` up to `fetch_limit`. A path with syntax facts answers through its
/// provider-backed file; every other path answers through baseline text file.
fn collect_indexed_content_hits(
    index: &WorkspaceIndex,
    matcher: Option<&PathMatcher>,
    root: &Path,
    query: &str,
    fetch_limit: usize,
    payloads: HitPayloads,
    results: &mut Vec<SearchHit>,
) -> Result<(), ReadError> {
    for (file, line, text) in index
        .text_matches(query, fetch_limit)
        .map_err(ReadFault::index)?
    {
        if !includes(matcher, root, file.path()) {
            continue;
        }
        if let Some(provider_file) = index.file(file.path()) {
            results.push(file_search_hit(provider_file, line, text, payloads));
        } else {
            results.push(text_search_hit(file, line, text, payloads));
        }
        if results.len() >= fetch_limit {
            break;
        }
    }
    Ok(())
}

/// Reaches request-selected files outside persistent index.
fn collect_force_include_hits(
    index: &WorkspaceIndex,
    selector: &PathSelector,
    criteria: SearchCriteria<'_>,
    fetch_limit: usize,
    results: &mut Vec<SearchHit>,
) -> Result<(), ReadError> {
    if selector.force_include.is_empty() {
        return Ok(());
    }
    let SearchCriteria {
        query,
        target,
        payloads,
    } = criteria;
    let extra = index
        .force_include_index(
            &pattern_strings(&selector.force_include),
            FORCE_INCLUDE_FILES_MAX,
        )
        .map_err(ReadFault::index)?;
    if matches!(target, SearchParamsTarget::All | SearchParamsTarget::Symbol) {
        for matched in extra
            .symbols(query, fetch_limit - results.len())
            .map_err(ReadFault::index)?
        {
            results.push(symbol_search_hit(&extra, matched, payloads)?);
        }
    }
    if results.len() < fetch_limit
        && matches!(target, SearchParamsTarget::All | SearchParamsTarget::File)
    {
        for (file, line, text) in
            rift_index::source_line_matches(extra.files(), query, fetch_limit - results.len())
        {
            results.push(file_search_hit(file, line, text, payloads));
        }
    }
    if results.len() < fetch_limit
        && matches!(target, SearchParamsTarget::All | SearchParamsTarget::File)
    {
        for (file, line, text) in extra
            .text_matches(query, fetch_limit - results.len())
            .map_err(ReadFault::index)?
        {
            if extra.file(file.path()).is_none() {
                results.push(text_search_hit(file, line, text, payloads));
            }
        }
    }
    Ok(())
}

fn symbol_search_hit(
    index: &WorkspaceIndex,
    matched: SymbolMatch<'_>,
    payloads: HitPayloads,
) -> Result<SearchHit, ReadError> {
    let score = symbol_match_score(matched.rank);
    build_symbol_hit(index, matched, score, vec![MatchedField::Name], payloads)
}

/// Builds one symbol hit's wire shape. `symbol_search_hit` and `merge_symbol_hit` share
/// this: both surface the same declaration, differing only in score and which indexed field
/// produced the match. The excerpt behind `source` is sliced only when `payloads` asked for
/// it, so a request that omits `include` never pays that lookup.
fn build_symbol_hit(
    index: &WorkspaceIndex,
    matched: SymbolMatch<'_>,
    score: f64,
    matched_by: Vec<MatchedField>,
    payloads: HitPayloads,
) -> Result<SearchHit, ReadError> {
    // A retained disagreement surfaces as a `symbol_disagreement` warning on `get_symbol`
    // (crates/rift-server/src/read.rs); doing the same for a search hit needs the same
    // warnings accumulator threaded through every collector this file merges hits
    // through, and no shipped provider produces a disagreement today. Scoped out of this
    // change; `get_symbol` already carries it.
    let (symbol, _disagreement) = wire_symbol(index, matched)?;
    Ok(SearchHit {
        hit: SearchHitTarget::Symbol {
            symbol: Box::new(symbol),
        },
        score: Some(score),
        matched_by,
        source: payloads
            .source
            .then(|| excerpt(matched.file, matched.symbol.range)),
        range: Some(text_range(matched.symbol.range)),
        line: Some(line::line_number_at(
            matched.file.source(),
            matched.symbol.range.start,
        )),
        path: Some(project_path(matched.file.path())),
        traversal_path: None,
        distance: None,
    })
}

/// Builds one file hit wire value.
fn file_search_hit(
    file: &IndexedFile,
    line_index: usize,
    text: String,
    payloads: HitPayloads,
) -> SearchHit {
    let start = line::line_start_offset(file.source(), line_index);
    let end = start.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
    let range = ByteRange { start, end };
    SearchHit {
        hit: SearchHitTarget::File {
            size: u64::try_from(file.source().len()).unwrap_or(u64::MAX),
            languages: vec![file.syntax().language().clone()],
        },
        score: Some(1.0),
        matched_by: vec![MatchedField::Content],
        source: payloads.source.then_some(text),
        range: Some(text_range(range)),
        line: Some(u64::try_from(line_index).unwrap_or(u64::MAX)),
        path: Some(project_path(file.path())),
        traversal_path: None,
        distance: None,
    }
}

/// Builds one baseline content file hit's target: a `[search.text]` file carries no
/// language claim, unlike a syntax-indexed file's own.
fn text_file_hit_target(file: &TextSourceFile) -> SearchHitTarget {
    SearchHitTarget::File {
        size: u64::try_from(file.content().len()).unwrap_or(u64::MAX),
        languages: Vec::new(),
    }
}

/// Builds one text-lane content-line hit's wire shape, the same shape [`file_search_hit`]
/// builds for a syntax-indexed file, over a `[search.text]` file instead.
fn text_search_hit(
    file: &TextSourceFile,
    line_index: usize,
    text: String,
    payloads: HitPayloads,
) -> SearchHit {
    let start = line::line_start_offset(file.content(), line_index);
    let end = start.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
    let range = ByteRange { start, end };
    SearchHit {
        hit: text_file_hit_target(file),
        score: Some(1.0),
        matched_by: vec![MatchedField::Content],
        source: payloads.source.then_some(text),
        range: Some(text_range(range)),
        line: Some(u64::try_from(line_index).unwrap_or(u64::MAX)),
        path: Some(project_path(file.path())),
        traversal_path: None,
        distance: None,
    }
}

const fn symbol_match_score(rank: SymbolMatchRank) -> f64 {
    match rank {
        SymbolMatchRank::QualifiedExact => 1.0,
        SymbolMatchRank::NameExact => 0.9,
        SymbolMatchRank::NamePrefix => 0.8,
        SymbolMatchRank::Substring => 0.7,
    }
}

/// Merges the search index's ranked units into `results`: a resolved symbol becomes a hit
/// at its fused score, and a text file's best-scoring chunk becomes a file hit whose line is
/// the first line containing a query term. Each merges against an identifier-matched hit
/// already in `results` by identity, rather than duplicating it. `matcher` and `root` drop
/// a ranked hit at an excluded path exactly as the indexed lanes already do.
///
/// The score arrives already fused into 0 to 1, so nothing here converts a rank: two
/// requests' scores mean the same thing because one fusion produced both.
fn collect_ranked_hits(
    index: &WorkspaceIndex,
    matcher: Option<&PathMatcher>,
    root: &Path,
    criteria: SearchCriteria<'_>,
    ranked: &[RankedUnit],
    results: &mut Vec<SearchHit>,
) -> Result<(), ReadError> {
    let SearchCriteria {
        query,
        target,
        payloads,
    } = criteria;
    if matches!(target, SearchParamsTarget::All | SearchParamsTarget::Symbol) {
        for matched in ranked
            .iter()
            .filter(|matched| matched.kind() == LexicalUnitKind::Symbol)
        {
            let Some((file, symbol)) = resolve_symbol(index, matched.path(), matched.identity())
            else {
                // The search index held this symbol at a tree revision the request's
                // revision guard already proved current, but a symbol it named can still be
                // gone from this exact index; skipping it silently is correct.
                continue;
            };
            if !includes(matcher, root, file.path()) {
                continue;
            }
            merge_symbol_hit(index, results, file, symbol, matched.score(), payloads)?;
        }
    }
    if matches!(target, SearchParamsTarget::All | SearchParamsTarget::File) {
        for (path, score) in best_score_per_text_file(ranked) {
            let Some(file) = index.text_file(&path) else {
                continue;
            };
            if !includes(matcher, root, file.path()) {
                continue;
            }
            let (line_number, range, text) = locate_query_line(file.content(), query);
            merge_file_hit(results, file, line_number, range, text, score, payloads);
        }
    }
    Ok(())
}

/// Resolves one ranked symbol unit's identity back to its declaration in `index`. `path`
/// narrows the search to the one file the unit named, so this stays a scan of that file's
/// own symbols rather than the whole index.
fn resolve_symbol<'a>(
    index: &'a WorkspaceIndex,
    path: &ProjectPath,
    identity: &str,
) -> Option<(&'a IndexedFile, &'a SyntaxSymbol)> {
    let file = index.file(path)?;
    let language_segment = file.syntax().language().identity_segment();
    let symbol = file.syntax().symbols().iter().find(|symbol| {
        rift_core::symbol_identity(
            &language_segment,
            file.path().as_str(),
            &symbol.qualified_name,
        ) == identity
    })?;
    Some((file, symbol))
}

/// Collapses ranked text-file units to one entry per path, keeping the best - the highest -
/// fused score when a file contributed more than one chunk.
fn best_score_per_text_file(ranked: &[RankedUnit]) -> Vec<(ProjectPath, f64)> {
    let mut best: Vec<(ProjectPath, f64)> = Vec::new();
    for matched in ranked
        .iter()
        .filter(|matched| matched.kind() == LexicalUnitKind::TextFile)
    {
        if let Some(entry) = best.iter_mut().find(|(path, _)| path == matched.path()) {
            entry.1 = entry.1.max(matched.score());
        } else {
            best.push((matched.path().clone(), matched.score()));
        }
    }
    best
}

/// Finds the first line of `content` containing any of `query`'s whitespace-split terms,
/// case-insensitively, byte-exact so its span survives a CRLF file unchanged. Falls back to
/// line 1 with a whole-file span when no line matches.
fn locate_query_line(content: &str, query: &str) -> (u64, ByteRange, String) {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut offset: u64 = 0;
    for (index, raw_line) in line::lines_inclusive(content).enumerate() {
        let text = line::without_ending(raw_line);
        if terms
            .iter()
            .any(|term| text.to_lowercase().contains(term.as_str()))
        {
            let start = offset;
            let end = start.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
            let line_number = u64::try_from(index + 1).unwrap_or(u64::MAX);
            return (line_number, ByteRange { start, end }, text.to_owned());
        }
        offset = offset.saturating_add(u64::try_from(raw_line.len()).unwrap_or(u64::MAX));
    }
    let end = u64::try_from(content.len()).unwrap_or(u64::MAX);
    (1, ByteRange { start: 0, end }, content.to_owned())
}

/// Merges one resolved ranked symbol unit: an identifier-matched hit for the same symbol
/// keeps its place and absorbs `score`; otherwise the ranked hit joins `results` new.
fn merge_symbol_hit(
    index: &WorkspaceIndex,
    results: &mut Vec<SearchHit>,
    file: &IndexedFile,
    symbol: &SyntaxSymbol,
    score: f64,
    payloads: HitPayloads,
) -> Result<(), ReadError> {
    let identity = SymbolId(rift_core::symbol_identity(
        &file.syntax().language().identity_segment(),
        file.path().as_str(),
        &symbol.qualified_name,
    ));
    let existing = results.iter_mut().find(|hit| {
        matches!(
            &hit.hit,
            SearchHitTarget::Symbol { symbol } if symbol.id.as_ref() == Some(&identity)
        )
    });
    if let Some(existing) = existing {
        absorb_ranked_match(existing, score);
        return Ok(());
    }
    let matched = SymbolMatch {
        file,
        symbol,
        // This caller supplies `score` directly and never reads identifier rank.
        rank: SymbolMatchRank::Substring,
    };
    results.push(build_symbol_hit(
        index,
        matched,
        score,
        vec![MatchedField::Ranked],
        payloads,
    )?);
    Ok(())
}

/// Merges one ranked text-file unit: an identifier-matched hit at the same path
/// keeps its place and absorbs `score`; otherwise the ranked hit joins `results` new.
fn merge_file_hit(
    results: &mut Vec<SearchHit>,
    file: &TextSourceFile,
    line_number: u64,
    range: ByteRange,
    text: String,
    score: f64,
    payloads: HitPayloads,
) {
    let path = project_path(file.path());
    let existing = results.iter_mut().find(|hit| {
        matches!(&hit.hit, SearchHitTarget::File { .. }) && hit.path.as_ref() == Some(&path)
    });
    if let Some(existing) = existing {
        absorb_ranked_match(existing, score);
        return;
    }
    results.push(ranked_file_hit(
        file,
        line_number,
        range,
        text,
        score,
        payloads,
    ));
}

/// Records that `existing` also matched through the ranked lane: adds
/// [`MatchedField::Ranked`] when absent, and raises its score to the better of the two. The
/// ranked lane fuses a lexical and a semantic tier into one score with no per-hit record of
/// which tier placed it, so it can never claim [`MatchedField::Content`] - that member stays
/// a claim the identifier or line matcher proved against literal bytes.
fn absorb_ranked_match(existing: &mut SearchHit, score: f64) {
    if !existing.matched_by.contains(&MatchedField::Ranked) {
        existing.matched_by.push(MatchedField::Ranked);
    }
    existing.score = Some(existing.score.map_or(score, |current| current.max(score)));
}

fn ranked_file_hit(
    file: &TextSourceFile,
    line_number: u64,
    range: ByteRange,
    text: String,
    score: f64,
    payloads: HitPayloads,
) -> SearchHit {
    SearchHit {
        hit: text_file_hit_target(file),
        score: Some(score),
        matched_by: vec![MatchedField::Ranked],
        source: payloads.source.then_some(text),
        range: Some(text_range(range)),
        line: Some(line_number),
        path: Some(project_path(file.path())),
        traversal_path: None,
        distance: None,
    }
}

/// Most distinct symbols one traversal may visit beyond `seed`. `RelationshipStore`'s
/// adjacency lists are themselves sorted, so a walk that breaches this bound stops
/// discovering new symbols and truncates the same way on every call over the same graph.
const TRAVERSAL_NODES_MAX: usize = 10_000;

/// Fourth hit-collection lane: a bounded relationship walk from `traversal.seed`, merged into
/// `results` alongside whatever the lexical and ranked lanes already placed there. Runs after
/// them so a symbol also reached by the walk absorbs it instead of duplicating it.
///
/// # Errors
///
/// Returns [`ReadError`] naming the binding capability when this workspace's relationship
/// store is empty because the binding provider never ran, and `not_found` naming a seed that
/// resolves to neither a relationship-store node nor a lexical declaration.
fn collect_traversal_hits(
    reads: &ReadService,
    matcher: Option<&PathMatcher>,
    root: &Path,
    traversal: &SearchTraversal,
    payloads: HitPayloads,
    results: &mut Vec<SearchHit>,
) -> Result<bool, ReadError> {
    let store = reads.relationships();
    if store.is_empty() && !reads.index().binding_enabled() {
        return Err(ReadFault::unsupported("binding"));
    }
    let seed = resolve_traversal_seed(reads, &traversal.seed)?;
    let walk = walk_traversal(store, &seed, traversal);
    for (identity, path) in walk.discovered {
        if traversal
            .to
            .as_ref()
            .is_some_and(|to| to.0 != identity.as_str())
        {
            continue;
        }
        let Some((file, symbol)) = resolve_graph_symbol(reads.index(), &identity) else {
            // A graph node with no lexical declaration in this snapshot: the store outlived
            // the file it was built from. Skipping it is the same choice `merge_symbol_hit`
            // makes for a ranked unit whose declaration is likewise gone.
            continue;
        };
        if !includes(matcher, root, file.path()) {
            continue;
        }
        merge_traversal_hit(reads.index(), results, file, symbol, path, payloads)?;
    }
    Ok(walk.truncated)
}

/// Resolves a traversal's `seed`, refusing `not_found` naming it when the identity exists
/// neither as a relationship-store node nor as a lexical declaration. A real, isolated
/// declaration with zero edges still resolves; its walk simply finds nothing.
fn resolve_traversal_seed(reads: &ReadService, seed: &SymbolId) -> Result<CoreSymbolId, ReadError> {
    let not_found = || ReadFault::not_found(seed.0.clone());
    let identity = CoreSymbolId::new(seed.0.clone()).map_err(|_error| not_found())?;
    let store = reads.relationships();
    let in_store = !store.outgoing(&identity).is_empty() || !store.incoming(&identity).is_empty();
    let has_lexical_record = resolve_graph_symbol(reads.index(), &identity).is_some();
    if in_store || has_lexical_record {
        Ok(identity)
    } else {
        Err(not_found())
    }
}

/// Resolves one graph-walked identity back to its declaration, the way a ranked lexical unit
/// resolves: parse the wire address the identity spells, then scan the addressed file for the
/// matching declaration.
fn resolve_graph_symbol<'a>(
    index: &'a WorkspaceIndex,
    identity: &CoreSymbolId,
) -> Option<(&'a IndexedFile, &'a SyntaxSymbol)> {
    let address = parse_symbol_address(identity.as_str()).ok()?;
    resolve_symbol(index, &address.path, identity.as_str())
}

/// One traversal walk's outcome: each discovered symbol with its shortest path, and whether
/// the node bound stopped the walk before the reachable graph was exhausted.
struct TraversalWalk {
    discovered: Vec<(CoreSymbolId, Vec<GraphHop>)>,
    truncated: bool,
}

/// The warning a truncated traversal walk attaches: the bound the walk hit, and the
/// narrowing that fits a walk under it.
fn traversal_truncation_warning() -> ReadWarning {
    ReadWarning::TraversalTruncated {
        visited: TRAVERSAL_NODES_MAX as u64,
        detail: format!(
            "the traversal walk stopped at its {TRAVERSAL_NODES_MAX}-symbol bound; \
             narrow facets, lower depth, or start from a less-connected seed to walk \
             the rest"
        ),
    }
}

/// Walks `store` breadth-first from `seed`, honoring `traversal`'s direction, facet filter,
/// and depth bound. BFS visits each symbol at its shortest path first and never requeues a
/// visited symbol, so every returned path is the shortest one `store` has to it. Bounded by
/// [`TRAVERSAL_NODES_MAX`].
fn walk_traversal(
    store: &RelationshipStore,
    seed: &CoreSymbolId,
    traversal: &SearchTraversal,
) -> TraversalWalk {
    walk_traversal_capped(store, seed, traversal, TRAVERSAL_NODES_MAX)
}

/// [`walk_traversal`] under an explicit node cap, so a test can force truncation without a
/// graph sized past [`TRAVERSAL_NODES_MAX`] - the same split [`RelationshipStore::build`] and
/// its own `build_capped` use.
fn walk_traversal_capped(
    store: &RelationshipStore,
    seed: &CoreSymbolId,
    traversal: &SearchTraversal,
    nodes_max: usize,
) -> TraversalWalk {
    let mut visited: BTreeSet<CoreSymbolId> = BTreeSet::from([seed.clone()]);
    let mut budget = LoopBudget::new(nodes_max);
    let mut truncated = false;
    let mut queue: VecDeque<(CoreSymbolId, Vec<GraphHop>)> =
        VecDeque::from([(seed.clone(), Vec::new())]);
    let mut discovered = Vec::new();
    while let Some((current, path)) = queue.pop_front() {
        if path.len() as u64 >= traversal.depth {
            continue;
        }
        for (edge, hop_direction) in traversal_edges(store, &current, traversal.direction) {
            if !traversal.facets.is_empty() && !traversal.facets.contains(&edge.facet()) {
                continue;
            }
            let next = match hop_direction {
                HopDirection::Outgoing => edge.to().clone(),
                HopDirection::Incoming => edge.from().clone(),
            };
            if visited.contains(&next) {
                continue;
            }
            if budget.consume().is_err() {
                truncated = true;
                continue;
            }
            visited.insert(next.clone());
            let mut next_path = path.clone();
            next_path.push(graph_hop(edge, hop_direction));
            queue.push_back((next.clone(), next_path.clone()));
            discovered.push((next, next_path));
        }
    }
    TraversalWalk {
        discovered,
        truncated,
    }
}

/// The edges a search traversal walks from `current`, tagged with the [`HopDirection`] each
/// was followed relative to its own stored orientation: `outgoing` yields edges leaving
/// `current`; `incoming` yields edges arriving at it; `both` yields every outgoing edge before
/// every incoming edge, keeping the walk deterministic.
fn traversal_edges<'a>(
    store: &'a RelationshipStore,
    current: &CoreSymbolId,
    direction: TraversalDirection,
) -> Vec<(&'a RelationshipEdge, HopDirection)> {
    let mut edges = Vec::new();
    if matches!(
        direction,
        TraversalDirection::Outgoing | TraversalDirection::Both
    ) {
        edges.extend(
            store
                .outgoing(current)
                .iter()
                .map(|edge| (edge, HopDirection::Outgoing)),
        );
    }
    if matches!(
        direction,
        TraversalDirection::Incoming | TraversalDirection::Both
    ) {
        edges.extend(
            store
                .incoming(current)
                .iter()
                .map(|edge| (edge, HopDirection::Incoming)),
        );
    }
    edges
}

/// Builds one traversal step's wire relationship from the edge it followed. `RelationshipStore`
/// retains only a reference's portable facet, not the provider's original exact kind, so
/// `kind` is the facet's own serde spelling: the binding lane's exact kind IS its portable
/// facet spelling, the least invented value this lane can honestly report.
fn graph_hop(edge: &RelationshipEdge, direction: HopDirection) -> GraphHop {
    GraphHop {
        relationship: Relationship {
            from: wire_symbol_id(edge.from()),
            kind: ExactKind(rift_core::fault_label(&edge.facet())),
            facets: vec![edge.facet()],
            to: wire_symbol_id(edge.to()),
            evidence: edge.occurrence().node().cloned().into_iter().collect(),
            derivation: RelationshipDerivation::Resolution,
            confidence: None,
            extensions: Extensions::default(),
        },
        direction,
    }
}

fn wire_symbol_id(identity: &CoreSymbolId) -> SymbolId {
    SymbolId(identity.as_str().to_owned())
}

/// A reached symbol's `relevance` score: a closer hit (`distance` 1) scores higher than a
/// farther one (`distance` 2, `SearchTraversal`'s own bound). This is the whole ranking basis
/// a traversal-only request has, so ordering by it reproduces the distance-ascending order
/// `SearchTraversal`'s doc comment promises; a hit also matched lexically keeps its lexical
/// score instead - `absorb_traversal_match` never touches `score`.
fn traversal_hit_score(distance: u64) -> f64 {
    match distance {
        1 => 1.0,
        _ => 0.5,
    }
}

/// Merges one reached symbol: an existing hit for the same identity absorbs the walk;
/// otherwise the traversal places a new symbol hit tagged [`MatchedField::Relationship`].
fn merge_traversal_hit(
    index: &WorkspaceIndex,
    results: &mut Vec<SearchHit>,
    file: &IndexedFile,
    symbol: &SyntaxSymbol,
    path: Vec<GraphHop>,
    payloads: HitPayloads,
) -> Result<(), ReadError> {
    let distance = u64::try_from(path.len()).unwrap_or(u64::MAX);
    let identity = SymbolId(rift_core::symbol_identity(
        &file.syntax().language().identity_segment(),
        file.path().as_str(),
        &symbol.qualified_name,
    ));
    let existing = results.iter_mut().find(|hit| {
        matches!(
            &hit.hit,
            SearchHitTarget::Symbol { symbol } if symbol.id.as_ref() == Some(&identity)
        )
    });
    if let Some(existing) = existing {
        absorb_traversal_match(existing, path, distance);
        return Ok(());
    }
    let matched = SymbolMatch {
        file,
        symbol,
        // This lane supplies `score` from `distance` and never reads identifier rank.
        rank: SymbolMatchRank::Substring,
    };
    let mut hit = build_symbol_hit(
        index,
        matched,
        traversal_hit_score(distance),
        vec![MatchedField::Relationship],
        payloads,
    )?;
    hit.traversal_path = Some(path);
    hit.distance = Some(distance);
    results.push(hit);
    Ok(())
}

/// Records that `existing` was also reached by the walk: adds [`MatchedField::Relationship`]
/// when absent, and attaches the walk's path and distance. `existing.score` stays untouched -
/// a lexical or ranked match's score already means more than a graph distance would.
fn absorb_traversal_match(existing: &mut SearchHit, path: Vec<GraphHop>, distance: u64) {
    if !existing.matched_by.contains(&MatchedField::Relationship) {
        existing.matched_by.push(MatchedField::Relationship);
    }
    existing.traversal_path = Some(path);
    existing.distance = Some(distance);
}

/// Sorts merged hits by the request's `order`, every order ending in a hit's own stable wire
/// identity so the result does not depend on the arrival order of lexical matches, which
/// carries no guaranteed order of its own, and so two results that tie never swap places
/// between pages.
fn order_hits(results: &mut [SearchHit], order: ResultOrder) {
    results.sort_by(|left, right| hit_ordering(left, right, order));
}

fn hit_ordering(left: &SearchHit, right: &SearchHit, order: ResultOrder) -> Ordering {
    match order {
        ResultOrder::Relevance => right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| hit_identity(left).cmp(hit_identity(right))),
        ResultOrder::Path => left
            .path
            .cmp(&right.path)
            .then_with(|| hit_identity(left).cmp(hit_identity(right))),
        ResultOrder::Identity => hit_identity(left).cmp(hit_identity(right)),
    }
}

/// The wire identity `order_hits` breaks a `relevance` or `path` tie on.
fn hit_identity(hit: &SearchHit) -> &str {
    match &hit.hit {
        SearchHitTarget::Symbol { symbol } => symbol
            .id
            .as_ref()
            .map_or(symbol.name.as_str(), |identity| identity.0.as_str()),
        // Every file hit is built with its project path set; the empty fallback
        // only orders a hit no constructor in this crate produces.
        SearchHitTarget::File { .. } => hit.path.as_ref().map_or("", |path| path.0.as_str()),
        SearchHitTarget::Node { node } => node.0.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use std::sync::Arc;

    use rift_core::constants::READ_RESULTS_MAX_DEFAULT;
    use rift_core::{LanguageFileSelections, SourceVisibility};
    use rift_index::{
        BindingPolicy, LexicalIndexLimits, LexicalUnitKind, RelationshipStore, WorkspaceIndexLimits,
    };
    use rift_protocol::configuration::{BindingConfiguration, HistoryConfiguration};
    use rift_protocol::read::{
        MatchedField, NodeId, ReadWarning, RelationshipFacet, ResultOrder, SearchParams,
        SearchParamsTarget, SearchTraversal, TraversalDirection,
    };
    use rift_provider::{
        NormalizedGraph, Normalizer, ProviderPublication, PublicationLimits, PublicationSet,
    };
    use rift_search::{RankedUnit, SearchIndex, SearchIndexLimits};
    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        ByteRange, CoreSymbolId, ReadFault, ReadService, SearchHit, SearchHitTarget,
        SymbolMatchRank, TRAVERSAL_NODES_MAX, symbol_match_score, walk_traversal_capped,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub struct Beacon;\nimpl Beacon { pub fn signal(&self) {} }\n",
        )?;
        fs::write(directory.path().join("README.txt"), "Beacon docs")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    const RICH_SOURCE: &str = r#"pub enum Level { Low, High }

pub trait Speaks {
    fn say(&self);
}

pub type Alias = u32;

pub const MAX: u32 = 10;

pub static NAME: &str = "beacon";

pub mod inner {
    pub fn nested() {}
}

macro_rules! noop {
    () => {};
}

struct Hidden;

pub(crate) fn scoped() {}

pub fn compute() -> i32 {
    // lookout marker
    let total = 1 + 2;
    total;
    0
}
"#;

    fn rich_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), RICH_SOURCE)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    /// Three files under `src/`, so `*` vs `**` and include/exclude composition all have
    /// something to disagree about.
    fn multi_file_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src/nested"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn beacon_top() {}\n",
        )?;
        fs::write(
            directory.path().join("src/nested/deep.rs"),
            "pub fn beacon_nested() {}\n",
        )?;
        fs::write(
            directory.path().join("other.rs"),
            "pub fn beacon_other() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    /// One indexed file, one hidden by `.gitignore`, one hidden by `[source].exclude`, and one
    /// under the hard floor (`.git/`) - the fixture `force_include` tests reach into.
    fn force_include_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".gitignore"), "gitignored.rs\n")?;
        fs::write(
            directory.path().join("visible.rs"),
            "pub fn visible_symbol() {}\n",
        )?;
        fs::write(
            directory.path().join("gitignored.rs"),
            "pub fn phantom_gitignored() {}\n",
        )?;
        fs::write(
            directory.path().join("configured_out.rs"),
            "pub fn phantom_configured() {}\n",
        )?;
        fs::create_dir_all(directory.path().join(".git"))?;
        fs::write(
            directory.path().join(".git/floor.rs"),
            "pub fn floor() {}\n",
        )?;
        let visibility =
            SourceVisibility::new(Vec::new(), vec!["configured_out.rs".to_owned()], true);
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    /// Symbol hits from a TypeScript file carry the `typescript` language
    /// and its composed wire kind.
    #[test]
    fn search_symbol_hits_carry_the_typescript_language() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("routes.ts"),
            "export interface Route {\n  path: string;\n}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Route",
            "target": "symbol",
            "limit": 5
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        let symbol = &results[0]["hit"]["symbol"];
        assert_eq!(symbol["language"], json!("typescript"));
        assert_eq!(symbol["kind"], json!("interface"));
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/typescript/routes.ts/Route")
        );
        Ok(())
    }

    /// Symbol hits from a markdown file carry the `markdown` language, the
    /// composed wire kind, and an id escaping the heading text.
    #[test]
    fn search_symbol_hits_carry_the_markdown_language() -> TestResult {
        let directory = tempfile::tempdir()?;
        let notes_md = "# Beacon Notes\n\nCalibration steps.\n";
        fs::write(directory.path().join("notes.md"), notes_md)?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service =
            ReadService::build(directory.path(), limits, &visibility, &inclusion, history)?;
        let request = json!({
            "query": "Beacon Notes",
            "target": "symbol",
            "limit": 5
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        let symbol = &results[0]["hit"]["symbol"];
        assert_eq!(symbol["language"], json!("markdown"));
        assert_eq!(symbol["kind"], json!("heading"));
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/markdown/notes.md/Beacon%20Notes")
        );
        Ok(())
    }

    /// Symbol hits from JSON and YAML files carry their languages, the
    /// composed wire kinds, and ids escaping the key path.
    #[test]
    fn search_symbol_hits_carry_the_json_and_yaml_languages() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("config.json"),
            "{\"beacon port\": 8080}\n",
        )?;
        fs::write(directory.path().join("deploy.yaml"), "beacon retries: 3\n")?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service =
            ReadService::build(directory.path(), limits, &visibility, &inclusion, history)?;
        let expectations = [
            (
                "beacon port",
                "json",
                "member",
                "rift://symbol/json/config.json/beacon%20port",
            ),
            (
                "beacon retries",
                "yaml",
                "mapping_entry",
                "rift://symbol/yaml/deploy.yaml/beacon%20retries",
            ),
        ];
        for (query, language, kind, id) in expectations {
            let request = json!({
                "query": query,
                "target": "symbol",
                "limit": 5
            });
            let params: SearchParams = serde_json::from_value(request)?;
            let value = serde_json::to_value(service.search(&params, &[])?)?;
            let results = value["results"].as_array().ok_or("results must be array")?;
            assert!(!results.is_empty(), "{query} must return a hit");
            let symbol = &results[0]["hit"]["symbol"];
            assert_eq!(symbol["language"], json!(language));
            assert_eq!(symbol["kind"], json!(kind));
            assert_eq!(symbol["id"], json!(id));
        }
        Ok(())
    }

    #[test]
    fn search_combines_symbol_and_source_matches_with_limit() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "limit": 4
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;

        assert_eq!(results.len(), 4);
        // The pool holds five candidates, all but one tied at score 1.0: two matching source
        // lines in `src/lib.rs`, the text-lane `README.txt`'s one matching content line, and
        // the struct symbol. A file hit's own identity is its plain `path`; a symbol hit's is
        // its `rift://symbol/...` id. The tie-break sorts `README.txt` first (`R` < `r`), the
        // `Beacon` struct symbol second (`rift://symbol/...` < `src/lib.rs`), then the two
        // `src/lib.rs` file hits, in the order they were matched; the `signal` method's
        // substring match on the qualified name `Beacon::signal` scores lower and lands on
        // the next page.
        assert_eq!(
            value["pagination"],
            json!({ "page_index": 0, "total_pages": 2 })
        );
        assert_eq!(results[0]["hit"]["target"], "file");
        assert_eq!(results[0]["path"], json!("README.txt"));
        assert_eq!(results[1]["hit"]["target"], "symbol");
        assert_eq!(results[1]["path"], json!("src/lib.rs"));
        assert_eq!(results[2]["hit"]["target"], "file");
        assert_eq!(results[2]["path"], json!("src/lib.rs"));
        assert_eq!(results[3]["hit"]["target"], "file");
        assert_eq!(results[3]["path"], json!("src/lib.rs"));
        assert!(
            results[1]["path"]
                .as_str()
                .is_some_and(|path| !path.is_empty()),
            "every hit must carry a non-empty project-relative path: {:#?}",
            results[1]
        );
        Ok(())
    }

    /// Baseline catalog stops once `results_max` is reached.
    #[test]
    fn search_baseline_catalog_stops_at_fetch_limit() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        let source = "// Beacon marker\npub fn foo() {}\n";
        fs::write(directory.path().join("src/lib.rs"), source)?;
        fs::write(directory.path().join("README.txt"), "Beacon docs\n")?;
        let limits = WorkspaceIndexLimits::new(10, 4_096, 8_192, 8, 1).expect("positive limits");
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service =
            ReadService::build(directory.path(), limits, &visibility, &inclusion, history)?;
        let request = json!({
            "query": "Beacon",
            "target": "file",
            "limit": 10
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(
            results.len(),
            1,
            "baseline catalog fills results_max with one candidate: \
             {results:#?}"
        );
        assert_eq!(results[0]["path"], json!("README.txt"));
        Ok(())
    }

    /// Baseline catalog breaks the moment `results_max` is reached, leaving a later matching
    /// candidate out of the pool - the pool never overshoots `results_max` even though
    /// `text_matches` itself is called with the full bound rather than the room left.
    #[test]
    fn search_baseline_catalog_leaves_later_candidate_out() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        let source = "// Beacon marker\npub fn foo() {}\n";
        fs::write(directory.path().join("src/lib.rs"), source)?;
        fs::write(directory.path().join("README.txt"), "Beacon docs\n")?;
        fs::write(directory.path().join("notes.txt"), "Beacon notes\n")?;
        let limits = WorkspaceIndexLimits::new(10, 4_096, 8_192, 8, 2).expect("positive limits");
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service =
            ReadService::build(directory.path(), limits, &visibility, &inclusion, history)?;
        let request = json!({
            "query": "Beacon",
            "target": "file",
            "limit": 10
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(
            results.len(),
            2,
            "baseline catalog stops the moment results_max is reached: {results:#?}"
        );
        let paths: Vec<_> = results.iter().map(|hit| hit["path"].clone()).collect();
        assert!(paths.contains(&json!("README.txt")));
        assert!(paths.contains(&json!("notes.txt")));
        assert!(
            !paths.contains(&json!("src/lib.rs")),
            "baseline catalog never reaches src/lib.rs once results_max is already spent: \
             {results:#?}"
        );
        Ok(())
    }

    /// A symbol hit's `path` used to render `null`, with the project-relative location
    /// reachable only by parsing the symbol's own id.
    #[test]
    fn search_hits_carry_project_relative_path_for_nested_files() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|hit| hit["path"].as_str().is_some_and(|path| !path.is_empty())),
            "every symbol hit must carry a non-empty project-relative path: {results:#?}"
        );
        assert!(
            results
                .iter()
                .any(|hit| hit["path"] == json!("src/nested/deep.rs")),
            "the nested file's hit must carry its nested project-relative path: {results:#?}"
        );
        Ok(())
    }

    /// The excerpt used to duplicate the hit's own `span` inside `source`; it now carries
    /// text only, and the range a caller needs is the one already on the hit.
    #[test]
    fn search_hit_source_excerpt_is_text_only_with_no_duplicate_span() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "target": "symbol",
            "include": ["source"],
            "limit": 1
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let hit = &value["results"][0];
        assert!(
            hit["source"].is_string(),
            "excerpt must serialize as a bare string, not an object carrying a span: {hit}"
        );
        assert_eq!(hit["source"], json!("pub struct Beacon;"));
        assert_eq!(hit["range"]["start"], 0);
        Ok(())
    }

    /// An omitted `include` never pays the `source` lookup: every hit still carries its
    /// symbol or file, `path`, `range`, and `line`, and none carries `source`.
    #[test]
    fn search_without_include_omits_source_but_keeps_symbol_path_span_and_line() -> TestResult {
        let (_directory, service) = fixture()?;
        let request = json!({ "query": "Beacon", "limit": 10 });
        let params: SearchParams = serde_json::from_value(request)?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(
            results.iter().all(|hit| hit["source"].is_null()),
            "an omitted include must never carry source: {results:#?}"
        );
        assert!(
            results.iter().all(|hit| !hit["path"].is_null()
                && !hit["range"].is_null()
                && !hit["line"].is_null()
                && !hit["hit"].is_null()),
            "an omitted include must still carry the hit's symbol or file, path, range, and \
             line: {results:#?}"
        );
        Ok(())
    }

    #[test]
    fn symbol_scores_preserve_semantic_rank() {
        let scores = [
            symbol_match_score(SymbolMatchRank::QualifiedExact),
            symbol_match_score(SymbolMatchRank::NameExact),
            symbol_match_score(SymbolMatchRank::NamePrefix),
            symbol_match_score(SymbolMatchRank::Substring),
        ];
        assert!(scores.windows(2).all(|pair| pair[0] > pair[1]));
    }

    /// `target: "node"` left `SearchParamsTarget`'s served variants; a request naming it is
    /// refused at deserialization, not accepted and silently ignored.
    #[test]
    fn search_target_node_is_refused_as_an_unknown_enum_value() {
        let result: Result<SearchParams, _> =
            serde_json::from_value(json!({"query": "Beacon", "target": "node"}));
        assert!(
            result.is_err(),
            "a withdrawn target value must fail deserialization"
        );
    }

    #[test]
    fn search_requires_query_rejects_empty_query_and_zero_limit() -> TestResult {
        let (_directory, service) = fixture()?;
        let missing_query: SearchParams = serde_json::from_value(json!({}))?;
        assert!(matches!(
            service
                .search(&missing_query, &[])
                .expect_err("missing query must fail")
                .fault(),
            ReadFault::Invalid { .. }
        ));

        let empty_query: SearchParams = serde_json::from_value(json!({"query": ""}))?;
        assert!(matches!(
            service
                .search(&empty_query, &[])
                .expect_err("empty query must fail")
                .fault(),
            ReadFault::Invalid { .. }
        ));

        let zero_limit: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "limit": 0}))?;
        let error = service
            .search(&zero_limit, &[])
            .expect_err("zero limit must fail");
        assert!(matches!(error.fault(), ReadFault::Invalid { .. }));
        assert_eq!(
            error.to_string(),
            "the request does not match the documented form: field limit, \
             violation zero; correct the reported field and resend the request"
        );
        Ok(())
    }

    /// Collection is bounded by `results_max` whatever the page size, so a `limit` above
    /// that bound simply serves the whole bounded result set as one page.
    #[test]
    fn search_limit_above_the_result_bound_serves_the_whole_set_on_one_page() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "target": "file",
            "limit": READ_RESULTS_MAX_DEFAULT as u64 + 1
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert_eq!(value["pagination"]["page_index"], json!(0));
        assert_eq!(value["pagination"]["total_pages"], json!(1));
        Ok(())
    }

    #[test]
    fn search_target_symbol_excludes_file_hits() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "target": "symbol",
            "limit": 5
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(results.iter().all(|hit| hit["hit"]["target"] == "symbol"));
        Ok(())
    }

    #[test]
    fn search_reports_multi_line_file_match_position() -> TestResult {
        let (_directory, service) = rich_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "lookout marker",
            "target": "file",
            "include": ["source"]
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 1);
        assert!(results[0]["line"].as_u64().is_some_and(|line| line > 1));
        assert_eq!(results[0]["source"], "    // lookout marker");
        Ok(())
    }

    /// A sentence present in both a provider-claimed file (markdown, syntax index) and a
    /// `.mdx` file (`[search.text]`, no provider claims it) returns both hits: the text-lane
    /// lexical lane used to reach a text file only through the semantic tier, so this
    /// sentence never reached the mdx file's hit before the lexical lane searched it too.
    #[test]
    fn search_returns_a_text_lane_hit_alongside_a_provider_claimed_hit_for_the_same_sentence()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let sentence = "agentic development toolkit";
        fs::write(
            directory.path().join("README.md"),
            format!("# Rift\n\nRift is an {sentence}.\n"),
        )?;
        fs::write(
            directory.path().join("guide.mdx"),
            format!("Rift is an {sentence} for editors.\n"),
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": sentence,
            "target": "file",
            "limit": 10
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        let readme = results
            .iter()
            .find(|hit| hit["path"] == json!("README.md"))
            .ok_or("the provider-claimed file must return a hit")?;
        assert_eq!(
            readme["hit"]["languages"],
            json!(["markdown"]),
            "a provider-claimed file carries the language that claimed it: {readme:#?}"
        );
        let mdx = results
            .iter()
            .find(|hit| hit["path"] == json!("guide.mdx"))
            .ok_or("the text-lane file must return a hit through the lexical lane")?;
        assert_eq!(
            mdx["hit"]["languages"],
            serde_json::Value::Null,
            "a text-lane file no provider claims carries no language: {mdx:#?}"
        );
        Ok(())
    }

    /// An explicit `[search.text]` path rule reaches an extensionless `justfile`, and a
    /// query for content only it holds returns it.
    #[test]
    fn search_returns_an_explicitly_included_justfile_hit() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("justfile"),
            "test:\n\tcargo test --workspace\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1 << 20),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "cargo test --workspace",
            "target": "file",
            "limit": 5
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(
            results.len(),
            1,
            "only the justfile holds this content: {results:#?}"
        );
        assert_eq!(results[0]["path"], json!("justfile"));
        assert_eq!(
            results[0]["hit"]["languages"],
            serde_json::Value::Null,
            "a baseline [search.text] file claims no language"
        );
        Ok(())
    }

    #[test]
    fn search_resolves_every_rust_symbol_kind_and_visibility() -> TestResult {
        let (_directory, service) = rich_fixture()?;
        let cases = [
            ("Level", "enum", None),
            ("Speaks", "trait", None),
            ("Alias", "type_alias", None),
            ("MAX", "constant", None),
            ("NAME", "static", None),
            ("inner", "module", None),
            ("noop", "macro", None),
            ("Hidden", "struct", Some("private")),
            ("scoped", "function", Some("pub(crate)")),
        ];
        for (name, kind, visibility) in cases {
            let params: SearchParams = serde_json::from_value(json!({
                "query": name,
                "target": "symbol",
                "limit": 1
            }))?;
            let value = serde_json::to_value(service.search(&params, &[])?)?;
            let hit = &value["results"][0]["hit"]["symbol"];
            assert_eq!(hit["kind"], kind, "unexpected kind for {name}");
            if let Some(expected_visibility) = visibility {
                assert_eq!(
                    hit["visibility"], expected_visibility,
                    "unexpected visibility for {name}"
                );
                assert!(
                    !hit["facets"]
                        .as_array()
                        .is_some_and(|facets| facets.contains(&json!("public"))),
                    "{name} must not carry public facet"
                );
            }
        }
        Ok(())
    }

    /// `filter` left the served schema; a request naming it is refused as an unknown field,
    /// alone or alongside `paths`. `traversal` is this PR's return: a well-formed request
    /// carrying it parses, alone or alongside `paths` and `query`.
    #[test]
    fn search_filter_stays_refused_and_traversal_now_parses_alone_and_with_paths() {
        let filter = json!({
            "kind": "field",
            "field": {"field": "name", "op": "eq", "value": "Beacon"}
        });
        let traversal = json!({"seed": "rift://symbol/rust/src/lib.rs/Beacon"});
        let paths = json!({"include": ["src/lib.rs"]});
        let refused = [
            json!({"query": "Beacon", "filter": filter.clone()}),
            json!({"query": "Beacon", "filter": filter, "paths": paths.clone()}),
        ];
        for case in refused {
            let result: Result<SearchParams, _> = serde_json::from_value(case.clone());
            assert!(
                result.is_err(),
                "a withdrawn field must fail deserialization: {case}"
            );
        }
        let accepted = [
            json!({"traversal": traversal.clone()}),
            json!({"query": "Beacon", "traversal": traversal, "paths": paths}),
        ];
        for case in accepted {
            let result: Result<SearchParams, _> = serde_json::from_value(case.clone());
            assert!(
                result.is_ok(),
                "a well-formed traversal request must parse: {case} ({result:?})"
            );
        }
    }

    #[test]
    fn search_query_with_paths_succeeds_end_to_end() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "paths": {"include": ["src/lib.rs"]}
        }))?;
        let result = service.search(&params, &[])?;
        assert!(!result.results.is_empty());
        Ok(())
    }

    #[test]
    fn search_paths_include_narrows_to_matching_files() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "paths": {"include": ["other.rs"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(results.iter().all(|hit| {
            hit["hit"]["symbol"]["id"]
                .as_str()
                .or_else(|| hit["path"].as_str())
                .is_some_and(|id| id.contains("other.rs"))
        }));
        Ok(())
    }

    #[test]
    fn search_paths_exclude_drops_matching_files() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10,
            "paths": {"exclude": ["other.rs"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(results.iter().all(|hit| {
            !hit["hit"]["symbol"]["id"]
                .as_str()
                .unwrap_or_default()
                .contains("other.rs")
        }));
        Ok(())
    }

    #[test]
    fn search_paths_include_and_exclude_compose() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10,
            "paths": {"include": ["src/**"], "exclude": ["src/nested/**"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 1);
        assert!(
            results[0]["hit"]["symbol"]["id"]
                .as_str()
                .is_some_and(|id| id.contains("src/lib.rs"))
        );
        Ok(())
    }

    #[test]
    fn search_paths_star_does_not_cross_slash() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10,
            "paths": {"include": ["src/*.rs"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 1);
        assert!(
            results[0]["hit"]["symbol"]["id"]
                .as_str()
                .is_some_and(|id| id.contains("src/lib.rs"))
        );
        Ok(())
    }

    #[test]
    fn search_paths_double_star_crosses_slash() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10,
            "paths": {"include": ["src/**"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 2);
        Ok(())
    }

    #[test]
    fn search_paths_target_file_returns_only_matching_tree_entries() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "file",
            "paths": {"include": ["src/lib.rs"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(results.iter().all(|hit| hit["hit"]["target"] == "file"));
        assert!(results.iter().all(|hit| hit["path"] == json!("src/lib.rs")));
        Ok(())
    }

    #[test]
    fn search_paths_include_invalid_glob_refuses() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "paths": {"include": ["["]}
        }))?;
        assert!(matches!(
            service
                .search(&params, &[])
                .expect_err("an invalid include glob must refuse")
                .fault(),
            ReadFault::Index(_)
        ));
        Ok(())
    }

    #[test]
    fn search_paths_backslash_pattern_refuses_before_reaching_the_glob_engine() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "paths": {"include": ["src\\lib.rs"]}
        }))?;
        assert!(matches!(
            service
                .search(&params, &[])
                .expect_err("a backslash pattern must be refused")
                .fault(),
            ReadFault::Invalid { field: "paths", .. }
        ));
        Ok(())
    }

    #[test]
    fn search_paths_scoped_results_form_stable_prefix_across_limits() -> TestResult {
        // `paths` filtering happens before one page is cut out of the ordered pool, so the
        // top hit of a `paths`-scoped search does not move as `limit` grows.
        let (_directory, service) = multi_file_fixture()?;
        let narrow: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "paths": {"include": ["**/*.rs"]},
            "limit": 1
        }))?;
        let wide: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "paths": {"include": ["**/*.rs"]},
            "limit": 3
        }))?;
        let narrow_value = serde_json::to_value(service.search(&narrow, &[])?)?;
        let wide_value = serde_json::to_value(service.search(&wide, &[])?)?;
        assert_eq!(narrow_value["results"][0], wide_value["results"][0]);
        Ok(())
    }

    #[test]
    fn search_pages_partition_the_ordered_pool_without_overlap() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let mut seen = Vec::new();
        for page_index in 0..3_u64 {
            let request = json!({
                "query": "beacon",
                "target": "symbol",
                "limit": 1,
                "page_index": page_index
            });
            let params: SearchParams = serde_json::from_value(request)?;
            let value = serde_json::to_value(service.search(&params, &[])?)?;
            assert_eq!(
                value["pagination"],
                json!({ "page_index": page_index, "total_pages": 3 })
            );
            let id = value["results"][0]["hit"]["symbol"]["id"]
                .as_str()
                .ok_or("every page must carry one symbol hit")?
                .to_owned();
            assert!(!seen.contains(&id), "pages must not overlap: {id}");
            seen.push(id);
        }
        Ok(())
    }

    #[test]
    fn search_page_past_the_end_is_empty_with_the_true_page_count() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let request = json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 1,
            "page_index": 30
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        assert_eq!(value["results"], json!([]));
        assert_eq!(
            value["pagination"],
            json!({ "page_index": 30, "total_pages": 3 })
        );
        Ok(())
    }

    #[test]
    fn search_order_path_sorts_by_project_path() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "order": "path",
            "limit": 10
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let paths: Vec<String> = value["results"]
            .as_array()
            .ok_or("results must be array")?
            .iter()
            .map(|hit| hit["path"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(paths, ["other.rs", "src/lib.rs", "src/nested/deep.rs"]);
        Ok(())
    }

    #[test]
    fn search_order_identity_sorts_by_the_hits_own_identity() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "order": "identity",
            "limit": 10
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let ids: Vec<String> = value["results"]
            .as_array()
            .ok_or("results must be array")?
            .iter()
            .map(|hit| {
                hit["hit"]["symbol"]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            ids, sorted,
            "identity order sorts purely by each hit's own id, independent of score"
        );
        Ok(())
    }

    /// Two symbols in one file tie on `order: "path"`; the tie breaks on identity, and that
    /// relative order survives whether the page is cut at `limit: 1` or `limit: 2`.
    #[test]
    fn search_order_path_keeps_tied_hits_in_the_same_relative_order_across_page_sizes() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn beacon_alpha() {}\npub fn beacon_beta() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let mut narrow_ids = Vec::new();
        for page_index in 0..2_u64 {
            let request = json!({
                "query": "beacon",
                "target": "symbol",
                "order": "path",
                "limit": 1,
                "page_index": page_index
            });
            let params: SearchParams = serde_json::from_value(request)?;
            let value = serde_json::to_value(service.search(&params, &[])?)?;
            narrow_ids.push(
                value["results"][0]["hit"]["symbol"]["id"]
                    .as_str()
                    .ok_or("each narrow page must carry one hit")?
                    .to_owned(),
            );
        }
        let wide: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "order": "path",
            "limit": 2
        }))?;
        let wide_value = serde_json::to_value(service.search(&wide, &[])?)?;
        let wide_ids: Vec<String> = wide_value["results"]
            .as_array()
            .ok_or("results must be array")?
            .iter()
            .map(|hit| {
                hit["hit"]["symbol"]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            narrow_ids, wide_ids,
            "tied hits must keep their relative order across page sizes"
        );
        assert_eq!(
            wide_ids,
            [
                "rift://symbol/rust/src/lib.rs/beacon_alpha",
                "rift://symbol/rust/src/lib.rs/beacon_beta"
            ]
        );
        Ok(())
    }

    #[test]
    fn search_force_include_reaches_excluded_files_with_correct_path_and_span() -> TestResult {
        let (_directory, service) = force_include_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "phantom",
            "target": "symbol",
            "paths": {"force_include": ["gitignored.rs", "configured_out.rs"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 2);
        for hit in results {
            assert!(hit["range"]["start"].is_u64());
            let id = hit["hit"]["symbol"]["id"]
                .as_str()
                .ok_or("force_include hit must carry a symbol id")?;
            assert!(id.contains("gitignored.rs") || id.contains("configured_out.rs"));
        }
        Ok(())
    }

    #[test]
    fn search_force_include_matches_file_content_lines() -> TestResult {
        let (_directory, service) = force_include_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "phantom_gitignored",
            "target": "file",
            "paths": {"force_include": ["gitignored.rs"]},
            "include": ["source"]
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 1);
        let hit = &results[0];
        assert_eq!(hit["matched_by"][0], "content");
        let path = hit["path"]
            .as_str()
            .ok_or("content hit must carry a path")?;
        assert!(path.contains("gitignored.rs"));
        let text = hit["source"]
            .as_str()
            .ok_or("content hit must carry the matched line")?;
        assert!(text.contains("phantom_gitignored"));
        assert!(hit["line"].is_u64());
        Ok(())
    }

    #[test]
    fn search_force_include_of_indexed_file_does_not_duplicate_hits() -> TestResult {
        let (_directory, service) = force_include_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "visible_symbol",
            "target": "symbol",
            "paths": {"force_include": ["visible.rs"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(
            results.len(),
            1,
            "force_include of an already-indexed file must not duplicate its hit"
        );
        Ok(())
    }

    #[test]
    fn search_force_include_hard_floor_unreachable() -> TestResult {
        let (_directory, service) = force_include_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "floor",
            "paths": {"force_include": [".git/**"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(
            results.is_empty(),
            "the hard floor must stay unreachable via force_include"
        );
        Ok(())
    }

    #[test]
    fn search_force_include_bound_breach_refuses() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".gitignore"), "extra_*.rs\n")?;
        fs::write(directory.path().join("lib.rs"), "pub fn kept() {}\n")?;
        for index in 0..=super::FORCE_INCLUDE_FILES_MAX {
            fs::write(
                directory.path().join(format!("extra_{index:04}.rs")),
                "pub fn extra() {}\n",
            )?;
        }
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "extra",
            "paths": {"force_include": ["extra_*.rs"]}
        }))?;
        assert!(matches!(
            service
                .search(&params, &[])
                .expect_err("a force_include match count above the bound must refuse")
                .fault(),
            ReadFault::Index(_)
        ));
        Ok(())
    }

    #[test]
    fn search_force_include_invalid_glob_refuses() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "paths": {"force_include": ["["]}
        }))?;
        assert!(matches!(
            service
                .search(&params, &[])
                .expect_err("an invalid force_include glob must refuse")
                .fault(),
            ReadFault::Index(_)
        ));
        Ok(())
    }

    /// One committed source file, then uncommitted drift, so a revision
    /// search and a working-tree search answer differently.
    fn committed_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn committed_probe() {}\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "introduce probe");
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn drifted_probe() {}\n",
        )?;
        let service = ReadService::at_revision(
            directory.path(),
            &rift_protocol::read::RevisionId("main".to_owned()),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    #[test]
    fn search_at_a_revision_serves_committed_matches_only() -> TestResult {
        let (_directory, service) = committed_fixture()?;
        let committed: SearchParams = serde_json::from_value(json!({"query": "committed_probe"}))?;
        let value = serde_json::to_value(service.search(&committed, &[])?)?;
        let results = value["results"].as_array().ok_or("results array")?;
        assert!(!results.is_empty(), "the committed declaration matches");
        assert!(
            value.get("warnings").is_none(),
            "a search served from one index warns nothing, so warnings is omitted"
        );
        let drifted: SearchParams = serde_json::from_value(json!({"query": "drifted_probe"}))?;
        let drifted_value = serde_json::to_value(service.search(&drifted, &[])?)?;
        assert_eq!(
            drifted_value["results"].as_array().map(Vec::len),
            Some(0),
            "uncommitted drift is invisible at the revision"
        );
        Ok(())
    }

    #[test]
    fn search_finds_every_explicitly_included_utf8_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let files = [
            ("history.py", "python_catalog_marker"),
            ("worker.go", "go_catalog_marker"),
            ("notes.unknown", "unknown_catalog_marker"),
            ("buildfile", "extensionless_catalog_marker"),
        ];
        for (path, marker) in files {
            fs::write(directory.path().join(path), marker)?;
        }
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1 << 20),
            HistoryConfiguration::default(),
        )?;

        for (path, marker) in files {
            let params: SearchParams = serde_json::from_value(json!({
                "query": marker,
                "target": "file",
                "paths": { "include": [path] },
                "limit": 10
            }))?;
            let value = serde_json::to_value(service.search(&params, &[])?)?;
            assert_eq!(
                value["results"].as_array().map(Vec::len),
                Some(1),
                "one exact visible file must match: path={path}, value={value:#}"
            );
            assert_eq!(value["results"][0]["path"], json!(path));
        }
        Ok(())
    }

    #[tokio::test]
    async fn ranked_search_indexes_provider_file_content_once() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("calls.rs"),
            "fn caller() { wire_symbol(alpha, beta); }\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let database = directory.path().join("search.db");
        let ranked = ranked_units(&database, &service, "wire symbol beta").await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "wire symbol beta",
            "target": "file",
            "limit": 10
        }))?;
        let value = serde_json::to_value(service.search(&params, &ranked)?)?;
        let results = value["results"].as_array().ok_or("results array")?;
        assert_eq!(
            results.len(),
            1,
            "provider content must have one file identity: {results:#?}"
        );
        assert_eq!(results[0]["path"], json!("calls.rs"));
        assert!(
            ranked.iter().any(|unit| {
                unit.kind() == LexicalUnitKind::TextFile && unit.path().as_str() == "calls.rs"
            }),
            "provider content must join the baseline lexical units: {ranked:#?}"
        );
        Ok(())
    }

    #[test]
    fn revision_search_finds_visible_text_without_a_provider() -> TestResult {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::write(
            directory.path().join("rift_history.py"),
            "RIFT_HISTORY_PYTHON_MARKER\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "add history fixture");
        let service = ReadService::at_revision_with_languages(
            directory.path(),
            &rift_protocol::read::RevisionId("main".to_owned()),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1 << 20),
            &rift_core::LanguageFileSelections::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "RIFT_HISTORY_PYTHON_MARKER",
            "target": "file",
            "paths": { "include": ["rift_history.py"] }
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        assert_eq!(value["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(value["results"][0]["path"], json!("rift_history.py"));
        Ok(())
    }

    #[test]
    fn force_include_reaches_excluded_visible_text_without_a_provider() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".gitignore"), "hidden.py\n")?;
        fs::write(directory.path().join("hidden.py"), "FORCED_PYTHON_MARKER\n")?;
        fs::write(directory.path().join("visible.go"), "VISIBLE_GO_MARKER\n")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "FORCED_PYTHON_MARKER",
            "target": "file",
            "paths": {
                "include": ["*.py"],
                "exclude": ["visible.go"],
                "force_include": ["hidden.py"]
            }
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        assert_eq!(value["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(value["results"][0]["path"], json!("hidden.py"));
        Ok(())
    }

    #[test]
    fn binary_invalid_and_oversized_unknown_files_do_not_hide_valid_text() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("visible.py"), "VISIBLE_TEXT_MARKER\n")?;
        fs::write(directory.path().join("binary.unknown"), b"binary\0payload")?;
        fs::write(directory.path().join("invalid.unknown"), [0xff, 0xfe])?;
        fs::write(directory.path().join("oversized.unknown"), vec![b'x'; 33])?;
        let limits = WorkspaceIndexLimits::new(10, 32, 1_024, 8, 10)?;
        let service = ReadService::build(
            directory.path(),
            limits,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1 << 20),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "VISIBLE_TEXT_MARKER",
            "target": "file"
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        assert_eq!(value["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(value["results"][0]["path"], json!("visible.py"));
        Ok(())
    }

    #[test]
    fn search_at_a_revision_refuses_force_include() -> TestResult {
        let (_directory, service) = committed_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "committed_probe",
            "paths": {"force_include": ["lib.rs"]}
        }))?;
        let error = service.search(&params, &[]).expect_err(
            "force_include walks the working tree, which a revision search has none of",
        );
        assert!(matches!(
            error.fault(),
            ReadFault::Unsupported { capability }
            if capability == "force_include at a revision"
        ));
        Ok(())
    }

    fn indexed_symbol<'a>(
        service: &'a ReadService,
        path: &str,
        name: &str,
    ) -> TestResult<(&'a rift_index::IndexedFile, &'a rift_syntax::SyntaxSymbol)> {
        let file = service
            .index()
            .file(&rift_core::ProjectPath::new(path)?)
            .ok_or("fixture file must be indexed")?;
        let symbol = file
            .syntax()
            .symbols()
            .iter()
            .find(|symbol| symbol.name == name)
            .ok_or("fixture must declare the named symbol")?;
        Ok((file, symbol))
    }

    #[test]
    fn merge_symbol_hit_unions_matched_by_and_keeps_the_higher_score() -> TestResult {
        let (_directory, service) = fixture()?;
        let (file, symbol) = indexed_symbol(&service, "src/lib.rs", "Beacon")?;
        let existing = super::build_symbol_hit(
            service.index(),
            super::SymbolMatch {
                file,
                symbol,
                rank: SymbolMatchRank::NameExact,
            },
            0.5,
            vec![MatchedField::Name],
            super::HitPayloads::default(),
        )?;
        let mut results = vec![existing];

        super::merge_symbol_hit(
            service.index(),
            &mut results,
            file,
            symbol,
            0.9,
            super::HitPayloads::default(),
        )?;
        assert_eq!(results.len(), 1, "the same symbol must not duplicate");
        assert_eq!(results[0].score, Some(0.9), "the higher score must win");
        assert_eq!(
            results[0].matched_by,
            vec![MatchedField::Name, MatchedField::Ranked]
        );

        // A second merge at a lower score keeps the existing higher score and does not
        // duplicate the already-present Semantic field.
        super::merge_symbol_hit(
            service.index(),
            &mut results,
            file,
            symbol,
            0.1,
            super::HitPayloads::default(),
        )?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score, Some(0.9));
        assert_eq!(
            results[0].matched_by,
            vec![MatchedField::Name, MatchedField::Ranked]
        );
        Ok(())
    }

    /// One search index over `database`, semantic tier off, holding `service`'s own units,
    /// searched for `query`. A [`RankedUnit`] carries no constructor of its own, so the tier
    /// that produces them is the only way a test obtains one.
    async fn ranked_units(
        database: &std::path::Path,
        service: &ReadService,
        query: &str,
    ) -> TestResult<Vec<RankedUnit>> {
        let limits = SearchIndexLimits::builder(LexicalIndexLimits::default())
            .disable_semantic()
            .build();
        let index = SearchIndex::open(database, limits).await?;
        let units = service.lexical_units();
        let described = service.described_units(&units);
        let revision = service.tree_revision();
        index.replace_lexical(&units, revision).await?;
        index
            .embed_described(&described, rift_search::Embedding::Every, revision)
            .await?;
        let rift_search::RevisionScoped::Matched(ranked) =
            index.search(revision, query, 32).await?
        else {
            return Err("the store must hold the revision it was just stamped with".into());
        };
        assert!(!ranked.is_empty(), "the fixture query must rank something");
        Ok(ranked)
    }

    /// One workspace holding neither the fixture's declarations nor its text file, so every
    /// address the fixture ranked is one this index cannot resolve.
    fn unrelated_workspace() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("other.rs"), "pub fn unrelated() {}\n")?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text_inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let root = directory.path();
        let service = ReadService::build(root, limits, &visibility, &text_inclusion, history)?;
        Ok((directory, service))
    }

    #[test]
    fn described_units_pair_every_symbol_and_no_text_file_with_its_own_declaration() -> TestResult {
        let (_directory, service) = fixture()?;
        let units = service.lexical_units();
        let described = service.described_units(&units);
        let symbols: Vec<_> = units
            .iter()
            .filter(|unit| unit.kind() == LexicalUnitKind::Symbol)
            .collect();
        let texts = units
            .iter()
            .filter(|unit| unit.kind() == LexicalUnitKind::TextFile)
            .count();
        assert!(texts > 0, "the fixture must contribute a text-file unit");
        assert_eq!(
            described.len(),
            symbols.len(),
            "every symbol unit is described and no text-file unit is"
        );
        for one in &described {
            assert_eq!(
                one.unit().kind(),
                LexicalUnitKind::Symbol,
                "a text-file unit must never be described"
            );
            let identity = one.unit().identity();
            let text = rift_search::document(one.declaration()).into_text();
            assert!(
                text.contains(one.unit().content()),
                "each description must carry its own unit's declaration: {identity} {text}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn collect_ranked_hits_skips_a_symbol_identity_the_index_no_longer_carries() -> TestResult
    {
        let (directory, service) = fixture()?;
        let database = directory.path().join("search.db");
        let ranked = ranked_units(&database, &service, "Beacon").await?;
        assert!(
            ranked
                .iter()
                .any(|unit| unit.kind() == LexicalUnitKind::Symbol),
            "the fixture query must rank a symbol unit: {ranked:#?}"
        );
        let (_other, unrelated) = unrelated_workspace()?;
        let mut results = Vec::new();
        super::collect_ranked_hits(
            unrelated.index(),
            None,
            unrelated.index().root(),
            super::SearchCriteria {
                query: "Beacon",
                target: SearchParamsTarget::Symbol,
                payloads: super::HitPayloads::default(),
            },
            &ranked,
            &mut results,
        )?;
        assert!(
            results.is_empty(),
            "a symbol identity absent from the index must be skipped silently: {results:#?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn collect_ranked_hits_skips_a_text_file_path_the_index_no_longer_carries() -> TestResult
    {
        let (directory, service) = fixture()?;
        let database = directory.path().join("search.db");
        let ranked = ranked_units(&database, &service, "Beacon").await?;
        assert!(
            ranked
                .iter()
                .any(|unit| unit.kind() == LexicalUnitKind::TextFile),
            "the fixture query must rank the text file: {ranked:#?}"
        );
        let (_other, unrelated) = unrelated_workspace()?;
        let mut results = Vec::new();
        super::collect_ranked_hits(
            unrelated.index(),
            None,
            unrelated.index().root(),
            super::SearchCriteria {
                query: "Beacon",
                target: SearchParamsTarget::File,
                payloads: super::HitPayloads::default(),
            },
            &ranked,
            &mut results,
        )?;
        assert!(
            results.is_empty(),
            "a text-file path absent from the index must be skipped silently: {results:#?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn text_file_chunk_units_collapse_to_one_hit_at_the_best_score() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("guide.txt"), "word ".repeat(1000))?;
        // The smallest accepted chunk bound against a several-kilobyte guide forces the file
        // into more than one lexical unit, each of which the query matches.
        let text_inclusion = rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1_024);
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &text_inclusion,
            HistoryConfiguration::default(),
        )?;
        let database = directory.path().join("search.db");
        let ranked = ranked_units(&database, &service, "word").await?;
        let chunks = ranked
            .iter()
            .filter(|unit| unit.kind() == LexicalUnitKind::TextFile)
            .count();
        assert!(
            chunks > 1,
            "the oversized guide must rank more than one chunk: {ranked:#?}"
        );
        let best = ranked
            .iter()
            .filter(|unit| unit.kind() == LexicalUnitKind::TextFile)
            .map(rift_search::RankedUnit::score)
            .fold(f64::MIN, f64::max);
        let mut results = Vec::new();
        super::collect_ranked_hits(
            service.index(),
            None,
            service.index().root(),
            super::SearchCriteria {
                query: "word",
                target: SearchParamsTarget::File,
                payloads: super::HitPayloads::default(),
            },
            &ranked,
            &mut results,
        )?;
        assert_eq!(
            results.len(),
            1,
            "chunk units for the same file must collapse to one hit: {results:#?}"
        );
        assert_eq!(
            results[0].score,
            Some(best),
            "the best fused score must survive"
        );
        Ok(())
    }

    #[tokio::test]
    async fn search_merges_every_ranked_unit_while_the_semantic_tier_is_off() -> TestResult {
        let (directory, service) = fixture()?;
        let database = directory.path().join("search.db");
        let ranked = ranked_units(&database, &service, "Beacon").await?;
        let params: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "include": ["score"]}))?;
        let answer = service.search(&params, &ranked)?;
        for unit in &ranked {
            let path = unit.path().as_str();
            assert!(
                answer
                    .results
                    .iter()
                    .any(|hit| hit.path.as_ref().map(|found| found.0.as_str()) == Some(path)),
                "every ranked unit must reach the answer: path={path} answer={answer:#?}"
            );
        }
        assert!(
            answer
                .results
                .iter()
                .all(|hit| hit.score.is_some_and(|score| score > 0.0 && score <= 1.0)),
            "a fused score reaches the wire when requested, inside 0 to 1: {answer:#?}"
        );
        assert!(
            answer
                .results
                .iter()
                .any(|hit| hit.matched_by.contains(&MatchedField::Ranked)),
            "a ranked unit merges as a ranked match: {answer:#?}"
        );
        Ok(())
    }

    /// The lexical lane now searches a text-lane file's content directly, so `README.txt`
    /// reaches the answer through both the lexical lane (a literal `Beacon` in its content)
    /// and the ranked lane, and its hit carries both members - exactly as the exact `Beacon`
    /// struct match, reached through both the identifier matcher and the ranked lane, already
    /// does. [`merge_symbol_hit`] and [`merge_file_hit`]'s own tests prove the absorb behavior
    /// for a hit only one lane finds.
    #[tokio::test]
    async fn search_matched_by_carries_both_members_once_the_lexical_lane_covers_text_files()
    -> TestResult {
        let (directory, service) = fixture()?;
        let database = directory.path().join("search.db");
        let ranked = ranked_units(&database, &service, "Beacon").await?;
        let params: SearchParams = serde_json::from_value(json!({"query": "Beacon", "limit": 50}))?;
        let answer = service.search(&params, &ranked)?;

        let beacon = answer
            .results
            .iter()
            .find(|hit| {
                matches!(&hit.hit, SearchHitTarget::Symbol { symbol } if symbol.name == "Beacon")
            })
            .ok_or("the exact struct match must reach the answer")?;
        assert!(
            beacon.matched_by.contains(&MatchedField::Name)
                && beacon.matched_by.contains(&MatchedField::Ranked),
            "a hit both lanes found carries both members: {beacon:#?}"
        );

        let readme = answer
            .results
            .iter()
            .find(|hit| {
                matches!(&hit.hit, SearchHitTarget::File { .. })
                    && hit
                        .path
                        .as_ref()
                        .is_some_and(|path| path.0.contains("README"))
            })
            .ok_or("the text-lane file must reach the answer")?;
        assert!(
            readme.matched_by.contains(&MatchedField::Content)
                && readme.matched_by.contains(&MatchedField::Ranked),
            "the lexical lane finds README.txt's literal content and the ranked lane finds \
             it too, so the hit carries both members: {readme:#?}"
        );
        Ok(())
    }

    /// The only file holding the query's content sits at an excluded path; `paths.exclude`
    /// narrows the ranked lane exactly as it already narrows the indexed lanes, so the
    /// answer is empty rather than leaking the excluded text file's hit.
    #[tokio::test]
    async fn search_paths_exclude_narrows_the_ranked_lane_to_nothing() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("secret.txt"), "lighthouse guidance")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let database = directory.path().join("search.db");
        let ranked = ranked_units(&database, &service, "lighthouse").await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "lighthouse",
            "paths": {"exclude": ["secret.txt"]}
        }))?;
        let answer = service.search(&params, &ranked)?;
        assert!(
            answer.results.is_empty(),
            "an excluded path's only ranked match must not reach the answer: {answer:#?}"
        );
        Ok(())
    }

    #[test]
    fn merge_file_hit_absorbs_a_second_match_at_the_same_path_and_line() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("guide.txt"), "alpha units beta\n")?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text_inclusion = rift_core::TextFileInclusion::default();
        let service = ReadService::build(
            directory.path(),
            limits,
            &visibility,
            &text_inclusion,
            HistoryConfiguration::default(),
        )?;
        let file = service
            .index()
            .text_files()
            .next()
            .ok_or("fixture text file must be indexed")?;
        let range = ByteRange { start: 0, end: 17 };
        let mut results = Vec::new();

        super::merge_file_hit(
            &mut results,
            file,
            1,
            range,
            "alpha units beta".to_owned(),
            0.4,
            super::HitPayloads::default(),
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score, Some(0.4));
        assert_eq!(results[0].matched_by, vec![MatchedField::Ranked]);

        super::merge_file_hit(
            &mut results,
            file,
            1,
            range,
            "alpha units beta".to_owned(),
            0.9,
            super::HitPayloads::default(),
        );
        assert_eq!(
            results.len(),
            1,
            "a second match at the same path and line must not duplicate the hit"
        );
        assert_eq!(results[0].score, Some(0.9), "the higher score must win");
        assert_eq!(
            results[0].matched_by,
            vec![MatchedField::Ranked],
            "matched_by must union without duplicating an already-present field"
        );

        // A lower-scoring third match must not pull the absorbed score back down.
        super::merge_file_hit(
            &mut results,
            file,
            1,
            range,
            "alpha units beta".to_owned(),
            0.1,
            super::HitPayloads::default(),
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score, Some(0.9));
        Ok(())
    }

    /// Slices `content` at `range`'s byte offsets, for tests proving a returned span
    /// addresses exactly the bytes its accompanying text carries.
    fn byte_slice(content: &str, range: ByteRange) -> &str {
        let start = usize::try_from(range.start).expect("test fixture bytes must fit usize");
        let end = usize::try_from(range.end).expect("test fixture bytes must fit usize");
        &content[start..end]
    }

    #[test]
    fn locate_query_line_finds_first_matching_line_case_insensitively() {
        let content = "intro line\nSEARCH replace ALL units here\nend line\n";
        let (line_number, range, text) = super::locate_query_line(content, "replace all");
        assert_eq!(line_number, 2);
        assert_eq!(text, "SEARCH replace ALL units here");
        assert_eq!(
            byte_slice(content, range),
            text,
            "the returned span must address the exact matched line's bytes"
        );
    }

    #[test]
    fn locate_query_line_reports_byte_exact_spans_in_a_crlf_file() {
        let content = "one\r\ntwo replace\r\nthree\r\n";
        let (line_number, range, text) = super::locate_query_line(content, "replace");
        assert_eq!(line_number, 2);
        assert_eq!(text, "two replace");
        assert_eq!(byte_slice(content, range), "two replace");
    }

    #[test]
    fn locate_query_line_falls_back_to_a_whole_file_span_without_a_term_match() {
        let content = "alpha\nbeta\n";
        let (line_number, range, text) = super::locate_query_line(content, "gamma");
        assert_eq!(line_number, 1);
        assert_eq!(range.start, 0);
        assert_eq!(range.end, content.len() as u64);
        assert_eq!(text, content);
    }

    fn file_hit_stub(path: &str, score: f64) -> SearchHit {
        SearchHit {
            hit: SearchHitTarget::File {
                size: 0,
                languages: Vec::new(),
            },
            score: Some(score),
            matched_by: vec![MatchedField::Content],
            source: None,
            range: None,
            line: None,
            path: Some(rift_protocol::read::ProjectPath(path.to_owned())),
            traversal_path: None,
            distance: None,
        }
    }

    #[test]
    fn order_hits_relevance_sorts_by_score_then_a_deterministic_identity_tie_break() {
        let mut first_arrival = vec![
            file_hit_stub("rift://file/z.rs", 0.5),
            file_hit_stub("rift://file/b.rs", 0.9),
            file_hit_stub("rift://file/a.rs", 0.9),
        ];
        super::order_hits(&mut first_arrival, ResultOrder::Relevance);
        let ordered_ids: Vec<&str> = first_arrival.iter().map(super::hit_identity).collect();
        assert_eq!(
            ordered_ids,
            ["rift://file/a.rs", "rift://file/b.rs", "rift://file/z.rs"],
            "higher score sorts first; a tie breaks on ascending wire identity"
        );

        // The same three hits arriving in a different order sort to the identical result:
        // the tie-break is independent of arrival order, not merely input-stable.
        let mut second_arrival = vec![
            file_hit_stub("rift://file/b.rs", 0.9),
            file_hit_stub("rift://file/z.rs", 0.5),
            file_hit_stub("rift://file/a.rs", 0.9),
        ];
        super::order_hits(&mut second_arrival, ResultOrder::Relevance);
        let reordered_ids: Vec<&str> = second_arrival.iter().map(super::hit_identity).collect();
        assert_eq!(reordered_ids, ordered_ids);
    }

    /// `order: "path"` groups by project path, breaking a tie - two hits at the same path -
    /// on the hit's own wire identity, the same tie-break `relevance` uses. A file hit's
    /// own identity is its `path`, so two file hits can never tie this way; a node hit's
    /// identity is its own witnessed id, distinct from the path it also carries, so this
    /// proves the tie-break through node hits instead.
    #[test]
    fn order_hits_path_sorts_by_path_then_breaks_ties_on_identity() {
        let mut hits = vec![
            node_hit_stub_with_path("rift://node/rust/z.rs@0-1#00000000", Some("z.rs"), 0.9),
            node_hit_stub_with_path("rift://node/rust/a.rs@0-1#00000002", Some("a.rs"), 0.1),
            node_hit_stub_with_path("rift://node/rust/a.rs@0-1#00000001", Some("a.rs"), 0.5),
        ];
        super::order_hits(&mut hits, ResultOrder::Path);
        let ids: Vec<&str> = hits.iter().map(super::hit_identity).collect();
        assert_eq!(
            ids,
            [
                "rift://node/rust/a.rs@0-1#00000001",
                "rift://node/rust/a.rs@0-1#00000002",
                "rift://node/rust/z.rs@0-1#00000000",
            ],
            "path order groups by path; two hits at the same path break the tie on identity"
        );
    }

    /// `order: "identity"` sorts by each hit's own wire id alone, ignoring score.
    #[test]
    fn order_hits_identity_sorts_by_the_hits_own_identity_alone() {
        let mut hits = vec![
            file_hit_stub("rift://file/z.rs", 0.9),
            file_hit_stub("rift://file/a.rs", 0.1),
        ];
        super::order_hits(&mut hits, ResultOrder::Identity);
        let ids: Vec<&str> = hits.iter().map(super::hit_identity).collect();
        assert_eq!(ids, ["rift://file/a.rs", "rift://file/z.rs"]);
    }

    fn node_hit_stub(id: &str, score: f64) -> SearchHit {
        node_hit_stub_with_path(id, None, score)
    }

    fn node_hit_stub_with_path(id: &str, path: Option<&str>, score: f64) -> SearchHit {
        SearchHit {
            hit: SearchHitTarget::Node {
                node: NodeId(id.to_owned()),
            },
            score: Some(score),
            matched_by: vec![MatchedField::Content],
            source: None,
            range: None,
            line: None,
            path: path.map(|path| rift_protocol::read::ProjectPath(path.to_owned())),
            traversal_path: None,
            distance: None,
        }
    }

    /// `hit_identity`'s `Node` arm never runs through the live `search` path - a `target:
    /// "node"` request is refused before any hit is ever built - so this proves the arm
    /// directly: a `Node` hit tied in score with a `File` hit still breaks the tie on the
    /// node's own wire id, exactly as the `File` and `Symbol` arms already do.
    #[test]
    fn hit_identity_uses_the_node_id_as_tiebreak() {
        let mut results = vec![
            file_hit_stub("rift://file/z.rs", 0.5),
            node_hit_stub("rift://node/rust/lib.rs@0-1#00000000", 0.5),
        ];
        super::order_hits(&mut results, ResultOrder::Relevance);
        let ids: Vec<&str> = results.iter().map(super::hit_identity).collect();
        // "rift://file/..." sorts before "rift://node/..." lexicographically ('f' < 'n').
        assert_eq!(
            ids,
            ["rift://file/z.rs", "rift://node/rust/lib.rs@0-1#00000000",]
        );
    }

    // -- `walk_traversal` fixture: a small synthetic relationship graph, built through the
    // same public Contribution/NormalizedGraph API `RelationshipStore`'s own tests use, so
    // the walk's direction, facet, depth, and shortest-path behavior is proven without
    // depending on what a real syntax provider happens to classify a reference as.
    //
    //   root --calls--> branch_a --calls--> leaf
    //   root --calls--> branch_b --calls--> leaf
    //   root --imports--> helper

    fn graph_provider(value: &str) -> rift_core::ProviderId {
        rift_core::ProviderId::new(value).expect("fixture provider identity")
    }

    fn graph_source_unit(path: &str) -> rift_core::SourceUnitId {
        rift_core::SourceUnitId::new(
            rift_core::SourceResolverId::new("project").expect("fixture resolver identity"),
            rift_core::SourcePath::new(path).expect("fixture source path"),
        )
        .expect("fixture source unit")
    }

    fn graph_binding(path: &str, start: u64, end: u64) -> rift_core::DeclarationBinding {
        rift_core::DeclarationBinding::new(
            graph_source_unit(path),
            rift_core::SourceRange::new(start, end).expect("fixture range"),
            None,
        )
    }

    fn graph_exact() -> rift_core::SourceApplicability {
        rift_core::SourceApplicability::Exact {
            source_revision: rift_core::SourceRevision::new(1).expect("fixture source revision"),
            tree_revision: rift_core::TreeRevision::new(1).expect("fixture tree revision"),
        }
    }

    fn graph_authored_origin() -> rift_core::ContributionOrigin {
        rift_core::ContributionOrigin::new(
            Some(rift_core::SourceLocation::Project { package: None }),
            rift_core::SourceKind::Authored,
        )
        .expect("fixture origin")
    }

    /// One definition contribution: an established identity, anchored at `range` in
    /// `lib.rs`.
    fn graph_definition(
        symbol: &str,
        identity: &str,
        range: (u64, u64),
    ) -> rift_core::Contribution {
        let facts = rift_core::PortableSymbolFacts::new(
            rift_core::Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            identity,
            identity,
            rift_core::ExactKind("rust.function".to_owned()),
        );
        rift_core::Contribution::builder(
            rift_core::ContributionKey::new(
                graph_provider("syntax"),
                rift_core::ProviderRevision::new(1).expect("fixture provider revision"),
                rift_core::ProviderSymbolId::new(symbol).expect("fixture provider symbol"),
            ),
            graph_exact(),
            facts,
            graph_authored_origin(),
        )
        .source(graph_binding("lib.rs", range.0, range.1))
        .identity_anchor(rift_core::SymbolId::new(identity).expect("fixture identity"))
        .build()
        .expect("fixture definition contribution")
    }

    /// One reference-only contribution: `occurrence` sits inside its enclosing definition's
    /// range, so [`super::walk_traversal`]'s store carries the edge it produces.
    fn graph_reference(
        key_symbol: &str,
        occurrence: rift_core::DeclarationBinding,
        role: rift_core::ReferenceRole,
        target: &str,
    ) -> rift_core::Contribution {
        let target = rift_core::ContributionReference::new(
            graph_provider("syntax"),
            rift_core::ProviderSymbolId::new(target).expect("fixture target symbol"),
        );
        let semantic = rift_core::SemanticReference::new(occurrence, role, vec![target])
            .expect("fixture semantic reference");
        rift_core::Contribution::fact_builder(
            rift_core::ContributionKey::new(
                graph_provider("syntax"),
                rift_core::ProviderRevision::new(1).expect("fixture provider revision"),
                rift_core::ProviderSymbolId::new(key_symbol).expect("fixture provider symbol"),
            ),
            graph_exact(),
            graph_authored_origin(),
        )
        .references(vec![semantic])
        .build()
        .expect("fixture reference contribution")
    }

    fn graph_normalized(contributions: Vec<rift_core::Contribution>) -> NormalizedGraph {
        let publication = ProviderPublication::new(
            graph_provider("syntax"),
            rift_core::ProviderRevision::new(1).expect("fixture provider revision"),
            contributions,
            PublicationLimits::default(),
        )
        .expect("fixture publication");
        let publications = Arc::new(
            PublicationSet::empty(PublicationLimits::default())
                .replaced(publication)
                .expect("fixture publication set"),
        );
        Normalizer::normalize(
            rift_core::IndexRevision::new(1).expect("fixture index revision"),
            rift_core::SourceRevision::new(1).expect("fixture source revision"),
            rift_core::TreeRevision::new(1).expect("fixture tree revision"),
            &publications,
            None,
        )
        .expect("fixture normalized graph")
    }

    fn graph_symbol_id(text: &str) -> CoreSymbolId {
        CoreSymbolId::new(text).expect("fixture symbol identity")
    }

    /// `root` calls `branch_a` and `branch_b`, each of which calls `leaf`; `root` also
    /// imports `helper` directly, so one hop from `root` mixes a `calls` pair with one
    /// `imports` edge.
    fn call_graph_store() -> RelationshipStore {
        let contributions = vec![
            graph_definition("root", "rift://symbol/rust/lib.rs/root", (0, 40)),
            graph_definition("branch_a", "rift://symbol/rust/lib.rs/branch_a", (40, 80)),
            graph_definition("branch_b", "rift://symbol/rust/lib.rs/branch_b", (80, 120)),
            graph_definition("leaf", "rift://symbol/rust/lib.rs/leaf", (120, 160)),
            graph_definition("helper", "rift://symbol/rust/lib.rs/helper", (160, 200)),
            graph_reference(
                "root_calls_branch_a",
                graph_binding("lib.rs", 5, 10),
                rift_core::ReferenceRole::Call,
                "branch_a",
            ),
            graph_reference(
                "root_calls_branch_b",
                graph_binding("lib.rs", 15, 20),
                rift_core::ReferenceRole::Call,
                "branch_b",
            ),
            graph_reference(
                "root_imports_helper",
                graph_binding("lib.rs", 25, 30),
                rift_core::ReferenceRole::Import,
                "helper",
            ),
            graph_reference(
                "branch_a_calls_leaf",
                graph_binding("lib.rs", 45, 50),
                rift_core::ReferenceRole::Call,
                "leaf",
            ),
            graph_reference(
                "branch_b_calls_leaf",
                graph_binding("lib.rs", 85, 90),
                rift_core::ReferenceRole::Call,
                "leaf",
            ),
        ];
        RelationshipStore::build(&graph_normalized(contributions))
    }

    fn traversal_request(
        seed: &CoreSymbolId,
        direction: TraversalDirection,
        depth: u64,
        facets: Vec<RelationshipFacet>,
    ) -> SearchTraversal {
        SearchTraversal {
            seed: rift_protocol::read::SymbolId(seed.as_str().to_owned()),
            direction,
            facets,
            depth,
            to: None,
        }
    }

    #[test]
    fn walk_traversal_outgoing_reaches_both_direct_calls_and_records_outgoing_hops() {
        let store = call_graph_store();
        let root = graph_symbol_id("rift://symbol/rust/lib.rs/root");
        let request = traversal_request(&root, TraversalDirection::Outgoing, 1, vec![]);
        let discovered =
            walk_traversal_capped(&store, &root, &request, TRAVERSAL_NODES_MAX).discovered;
        let reached: Vec<&str> = discovered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            reached,
            [
                "rift://symbol/rust/lib.rs/branch_a",
                "rift://symbol/rust/lib.rs/branch_b",
                "rift://symbol/rust/lib.rs/helper",
            ],
            "outgoing order follows the store's own sorted adjacency"
        );
        for (_, path) in &discovered {
            assert_eq!(path.len(), 1);
            assert_eq!(path[0].direction, super::HopDirection::Outgoing);
            assert_eq!(path[0].relationship.from.0, root.as_str());
        }
    }

    #[test]
    fn walk_traversal_incoming_walks_edges_backward_and_keeps_their_natural_orientation() {
        let store = call_graph_store();
        let leaf = graph_symbol_id("rift://symbol/rust/lib.rs/leaf");
        let request = traversal_request(&leaf, TraversalDirection::Incoming, 1, vec![]);
        let discovered =
            walk_traversal_capped(&store, &leaf, &request, TRAVERSAL_NODES_MAX).discovered;
        let reached: Vec<&str> = discovered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            reached,
            [
                "rift://symbol/rust/lib.rs/branch_a",
                "rift://symbol/rust/lib.rs/branch_b",
            ]
        );
        for (reached_id, path) in &discovered {
            assert_eq!(path.len(), 1);
            assert_eq!(path[0].direction, super::HopDirection::Incoming);
            // The edge's own `from`/`to` stay in their stored orientation: walking backward
            // never swaps them.
            assert_eq!(path[0].relationship.from.0, reached_id.as_str());
            assert_eq!(path[0].relationship.to.0, leaf.as_str());
        }
    }

    #[test]
    fn walk_traversal_both_yields_every_outgoing_edge_before_every_incoming_edge() {
        let store = call_graph_store();
        let branch_a = graph_symbol_id("rift://symbol/rust/lib.rs/branch_a");
        let request = traversal_request(&branch_a, TraversalDirection::Both, 1, vec![]);
        let discovered =
            walk_traversal_capped(&store, &branch_a, &request, TRAVERSAL_NODES_MAX).discovered;
        let reached: Vec<(&str, super::HopDirection)> = discovered
            .iter()
            .map(|(id, path)| (id.as_str(), path[0].direction))
            .collect();
        assert_eq!(
            reached,
            [
                (
                    "rift://symbol/rust/lib.rs/leaf",
                    super::HopDirection::Outgoing
                ),
                (
                    "rift://symbol/rust/lib.rs/root",
                    super::HopDirection::Incoming
                ),
            ],
            "both walks every outgoing edge before every incoming edge"
        );
    }

    #[test]
    fn walk_traversal_facet_filter_keeps_only_the_named_facets() {
        let store = call_graph_store();
        let root = graph_symbol_id("rift://symbol/rust/lib.rs/root");
        let request = traversal_request(
            &root,
            TraversalDirection::Outgoing,
            1,
            vec![RelationshipFacet::Calls],
        );
        let discovered =
            walk_traversal_capped(&store, &root, &request, TRAVERSAL_NODES_MAX).discovered;
        let reached: Vec<&str> = discovered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            reached,
            [
                "rift://symbol/rust/lib.rs/branch_a",
                "rift://symbol/rust/lib.rs/branch_b",
            ],
            "the imports edge to helper must be excluded: {reached:?}"
        );
    }

    /// `leaf` is two hops from `root` down either branch; the walk keeps it once, at its
    /// shortest path, and `depth` 2 stops before a third hop could reach anything beyond it.
    #[test]
    fn walk_traversal_depth_bound_and_shortest_path_win_together() {
        let store = call_graph_store();
        let root = graph_symbol_id("rift://symbol/rust/lib.rs/root");
        let request = traversal_request(&root, TraversalDirection::Outgoing, 2, vec![]);
        let discovered =
            walk_traversal_capped(&store, &root, &request, TRAVERSAL_NODES_MAX).discovered;
        let leaf_hits: Vec<_> = discovered
            .iter()
            .filter(|(id, _)| id.as_str() == "rift://symbol/rust/lib.rs/leaf")
            .collect();
        assert_eq!(
            leaf_hits.len(),
            1,
            "leaf is reachable via two branches but must appear once: {discovered:?}"
        );
        assert_eq!(leaf_hits[0].1.len(), 2, "leaf's shortest path is two hops");
        assert_eq!(
            discovered.len(),
            4,
            "branch_a, branch_b, helper (depth 1) and leaf (depth 2), nothing past depth 2: \
             {discovered:?}"
        );
    }

    /// An explicit low cap truncates discovery deterministically, the way
    /// `RelationshipStore::build_capped` truncates edges: already-enqueued nodes still drain,
    /// but no new node is discovered once the budget is spent.
    #[test]
    fn walk_traversal_capped_stops_discovering_new_nodes_past_its_bound() {
        let store = call_graph_store();
        let root = graph_symbol_id("rift://symbol/rust/lib.rs/root");
        let request = traversal_request(&root, TraversalDirection::Outgoing, 2, vec![]);
        let walk = walk_traversal_capped(&store, &root, &request, 2);
        assert!(
            walk.truncated,
            "a walk stopped by its bound reports the truncation"
        );
        let discovered = walk.discovered;
        assert_eq!(
            discovered.len(),
            2,
            "a cap of 2 must discover exactly 2 new nodes: {discovered:?}"
        );
        let reached: Vec<&str> = discovered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            reached,
            [
                "rift://symbol/rust/lib.rs/branch_a",
                "rift://symbol/rust/lib.rs/branch_b",
            ],
            "the store's own sorted order makes the first two discoveries deterministic"
        );
    }

    #[test]
    fn traversal_truncation_warning_names_the_bound_it_hit() {
        let warning = super::traversal_truncation_warning();
        let ReadWarning::TraversalTruncated { visited, detail } = warning else {
            panic!("expected the traversal truncation variant: {warning:?}");
        };
        assert_eq!(visited, TRAVERSAL_NODES_MAX as u64);
        assert!(
            detail.contains(&TRAVERSAL_NODES_MAX.to_string()),
            "the detail names the bound: {detail}"
        );
    }

    #[test]
    fn walk_traversal_from_an_edgeless_seed_finds_nothing() {
        let store = call_graph_store();
        let helper = graph_symbol_id("rift://symbol/rust/lib.rs/helper");
        // `helper` has an incoming edge but no outgoing one.
        let request = traversal_request(&helper, TraversalDirection::Outgoing, 2, vec![]);
        let discovered =
            walk_traversal_capped(&store, &helper, &request, TRAVERSAL_NODES_MAX).discovered;
        assert!(discovered.is_empty(), "{discovered:?}");
    }

    // -- End-to-end `ReadService::search` coverage: capability refusal, seed resolution,
    // merges with lexical hits, `to`, and `target` interaction all need a real workspace,
    // since they resolve reached identities back through `WorkspaceIndex::file`.

    /// `root` calls `branch_a` and `branch_b`, each of which calls `leaf`, through the real
    /// Rust syntax and binding pipeline - a live twin of `call_graph_store`'s synthetic graph,
    /// used wherever a test needs a real `IndexedFile` a hit can resolve through.
    fn live_call_graph_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn root() {\n    branch_a();\n    branch_b();\n}\n\
             pub fn branch_a() {\n    leaf();\n}\n\
             pub fn branch_b() {\n    leaf();\n}\n\
             pub fn leaf() {}\n\
             pub fn isolated() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    fn binding_disabled_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let configuration = BindingConfiguration {
            enabled: false,
            ..BindingConfiguration::default()
        };
        let service = ReadService::build_with_languages(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            BindingPolicy::from(&configuration),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    #[test]
    fn search_traversal_reaches_a_neighbor_at_depth_one() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))?;
        let result = service.search(&params, &[])?;
        let hit = result
            .results
            .iter()
            .find(|hit| matches!(&hit.hit, SearchHitTarget::Symbol { symbol } if symbol.name == "branch_a"))
            .ok_or("branch_a must be reached at depth 1")?;
        assert_eq!(hit.distance, Some(1));
        assert!(
            hit.matched_by.contains(&MatchedField::Relationship),
            "{hit:?}"
        );
        assert!(!hit.matched_by.contains(&MatchedField::Name), "{hit:?}");
        Ok(())
    }

    #[test]
    fn search_traversal_to_keeps_only_the_reached_target_at_its_shortest_path() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "depth": 2,
                "to": "rift://symbol/rust/lib.rs/leaf"
            }
        }))?;
        let result = service.search(&params, &[])?;
        assert_eq!(result.results.len(), 1, "{:#?}", result.results);
        let hit = &result.results[0];
        assert!(matches!(
            &hit.hit,
            SearchHitTarget::Symbol { symbol } if symbol.name == "leaf"
        ));
        assert_eq!(hit.distance, Some(2));
        Ok(())
    }

    #[test]
    fn search_traversal_to_unreachable_within_depth_answers_empty_not_refused() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "to": "rift://symbol/rust/lib.rs/leaf"
            }
        }))?;
        let result = service.search(&params, &[])?;
        assert!(result.results.is_empty(), "{:#?}", result.results);
        Ok(())
    }

    #[test]
    fn search_traversal_target_file_answers_empty() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "target": "file",
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))?;
        let result = service.search(&params, &[])?;
        assert!(result.results.is_empty(), "{:#?}", result.results);
        Ok(())
    }

    #[test]
    fn search_query_and_traversal_merge_matched_by_and_keep_the_lexical_score() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        // "ranch_a" is a substring of "branch_a", not the name or qualified name itself, so
        // the lexical lane ranks it `Substring` (0.7) - a score distinct from what the
        // traversal lane would give the same hit at distance 1 (1.0), so a merge that kept
        // the traversal's score instead of the lexical one would show up here.
        let params: SearchParams = serde_json::from_value(json!({
            "query": "ranch_a",
            "target": "symbol",
            "include": ["score"],
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))?;
        let result = service.search(&params, &[])?;
        let hit = result
            .results
            .iter()
            .find(|hit| matches!(&hit.hit, SearchHitTarget::Symbol { symbol } if symbol.name == "branch_a"))
            .ok_or("branch_a must be a hit")?;
        assert!(hit.matched_by.contains(&MatchedField::Name), "{hit:?}");
        assert!(
            hit.matched_by.contains(&MatchedField::Relationship),
            "{hit:?}"
        );
        assert_eq!(
            hit.traversal_path.as_ref().map(Vec::len),
            Some(1),
            "the merged hit keeps the walk's path: {hit:?}"
        );
        assert_eq!(
            hit.score,
            Some(0.7),
            "the merge keeps the lexical Substring score rather than the traversal score: \
             {hit:?}"
        );
        Ok(())
    }

    #[test]
    fn search_traversal_seed_with_no_store_entry_and_no_declaration_is_not_found() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/ghost"
            }
        }))?;
        let error = service
            .search(&params, &[])
            .expect_err("an unresolvable seed must refuse");
        assert_eq!(error.descriptor().code(), "resource_not_found");
        Ok(())
    }

    /// `isolated` is a real declaration with zero edges: the walk finds nothing, but the
    /// seed itself resolves, so the request answers an empty result set rather than refusing.
    #[test]
    fn search_traversal_seed_with_no_edges_but_a_real_declaration_answers_empty() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/isolated"
            }
        }))?;
        let result = service.search(&params, &[])?;
        assert!(result.results.is_empty(), "{:#?}", result.results);
        Ok(())
    }

    #[test]
    fn search_traversal_refuses_capability_unavailable_when_binding_is_disabled() -> TestResult {
        let (_directory, service) = binding_disabled_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/beacon"
            }
        }))?;
        let error = service
            .search(&params, &[])
            .expect_err("a disabled binding provider must refuse the traversal lane");
        assert_eq!(error.descriptor().code(), "capability_unavailable");
        Ok(())
    }

    #[test]
    fn search_traversal_with_rev_refuses_capability_unavailable() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "rev": "HEAD",
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))?;
        let error = service.search(&params, &[]);
        // No git repository exists in this fixture, so revision resolution itself may refuse
        // first; either refusal proves `traversal` never silently combines with `rev`.
        assert!(error.is_err(), "a bare SearchParams cannot resolve `rev`");
        Ok(())
    }

    #[test]
    fn validate_search_refuses_traversal_with_rev_before_resolving_either() {
        let params: SearchParams = serde_json::from_value(json!({
            "rev": "main",
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))
        .expect("well-formed request must parse");
        let error =
            super::validate_search(&params).expect_err("traversal must refuse alongside rev");
        assert_eq!(error.descriptor().code(), "capability_unavailable");
    }

    /// `depth` and `facets.len()` carry `schemars` `range`/`length` constraints that serde
    /// deserialization never enforces on its own - the values below parse cleanly and only
    /// `validate_traversal` (through `validate_search`) refuses them.
    #[test]
    fn validate_search_refuses_traversal_depth_and_facets_out_of_bound() {
        let over_depth: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "depth": 3
            }
        }))
        .expect("an out-of-range depth still parses; the schema constraint is advisory only");
        let error = super::validate_search(&over_depth)
            .expect_err("depth 3 must be refused at the request-validation boundary");
        assert_eq!(error.descriptor().code(), "invalid_request");

        let padded_facets = vec![json!("calls"); super::SEARCH_TRAVERSAL_FACETS_MAX + 1];
        let over_facets: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "facets": padded_facets
            }
        }))
        .expect("a duplicate-padded facets list still parses");
        let error = super::validate_search(&over_facets)
            .expect_err("more facets than RelationshipFacet has variants must be refused");
        assert_eq!(error.descriptor().code(), "invalid_request");
    }
}
