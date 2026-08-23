//! `search` tool execution: lexical symbol and source-line search, narrowed or extended by a
//! request's `paths` selector. Extracted from `read` so that module stays below its size bound.

use std::cmp::Ordering;
use std::path::Path;

use rift_core::ProjectPath;
use rift_core::constants::{FORCE_INCLUDE_FILES_MAX, SEARCH_RESULTS_DEFAULT};
use rift_core::line;
use rift_index::{
    IndexedFile, LexicalMatch, LexicalUnitKind, PathMatcher, SymbolMatch, SymbolMatchRank,
    TextSourceFile, WorkspaceIndex,
};
use rift_protocol::read::{
    File, FileContent, MatchedField, PathPattern, PathSelector, SearchHit, SearchHitTarget,
    SearchParams, SearchParamsTarget, SearchResult, SymbolId,
};
use rift_syntax::{ByteRange, RustSymbol};

use crate::read::{
    ReadError, ReadFault, ReadService, accepted_limit, complete_coverage, excerpt, file_id,
    project_path, rust_language, source_span, validate_common, wire_symbol,
};

impl ReadService {
    /// Searches Rust declarations and source lines, optionally narrowed or extended by
    /// `params.paths`, merged with `lexical_matches` from the caller's lexical search-index
    /// tier. `lexical_matches` is empty when that tier is unavailable or its stamped
    /// revision no longer matches what is published - the caller decides that, this method
    /// only merges what it is handed.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for deferred filters, traversal, cursors, projections, scopes, an
    /// invalid `paths` glob, or a `force_include` bound crossed.
    pub fn search(
        &self,
        params: &SearchParams,
        lexical_matches: &[LexicalMatch],
    ) -> Result<SearchResult, ReadError> {
        validate_search(params)?;
        if self.revision().is_some() && force_include_requested(params) {
            return Err(ReadFault::unsupported("force_include at a revision"));
        }
        let query = params
            .query
            .as_deref()
            .ok_or_else(|| ReadFault::invalid("query", "missing"))?;
        if query.is_empty() {
            return Err(ReadFault::invalid("query", "empty"));
        }
        let limit = accepted_limit(params.limit.unwrap_or(SEARCH_RESULTS_DEFAULT as u64))?;
        let root = self.index().root();
        let selector = params.paths.as_ref();
        let matcher = path_matcher(root, selector)?;
        // Filtering happens before the page is truncated to `limit`, so a `paths`-narrowed
        // search still returns the true top-`limit` hits within the selected files rather than
        // an under-filled page: the candidate pool is fetched up to the index's own bound, then
        // filtered, then cut down to what the caller asked for.
        let fetch_limit = if matcher.is_some() {
            self.index().results_max()
        } else {
            limit
        };

        let mut results = Vec::new();
        collect_indexed_hits(
            self.index(),
            matcher.as_ref(),
            root,
            query,
            params.target,
            fetch_limit,
            &mut results,
        )?;
        if results.len() < limit
            && let Some(selector) = selector
        {
            collect_force_include_hits(
                self.index(),
                selector,
                query,
                params.target,
                limit,
                &mut results,
            )?;
        }
        collect_lexical_hits(
            self.index(),
            query,
            params.target,
            lexical_matches,
            &mut results,
        );
        order_by_relevance(&mut results);
        results.truncate(limit);
        Ok(SearchResult {
            coverage: complete_coverage(),
            results,
            next_cursor: None,
            warnings: self.warnings(),
        })
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

fn validate_search(params: &SearchParams) -> Result<(), ReadError> {
    validate_common(
        params.cursor.is_some(),
        params.projection.is_some(),
        params.rev.is_some(),
        params.scope,
    )?;
    if params.filter.is_some() || params.traversal.is_some() {
        return Err(ReadFault::unsupported("filtered and traversal search"));
    }
    if params.target == SearchParamsTarget::Node {
        return Err(ReadFault::unsupported("node search"));
    }
    if let Some(selector) = params.paths.as_ref() {
        validate_path_selector(selector)?;
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

/// Symbol and lexical hits from the index, filtered by `matcher` and collected up to
/// `fetch_limit` - never cut short at the smaller `limit` - because [`order_by_relevance`]
/// sorts this whole pool before the final page is cut down to `limit`; stopping collection
/// at `limit` could drop a later, higher-scoring candidate (a file hit scores 1.0 flat, so
/// this matters whenever a lower-scoring symbol substring match would otherwise fill the
/// page first).
fn collect_indexed_hits(
    index: &WorkspaceIndex,
    matcher: Option<&PathMatcher>,
    root: &Path,
    query: &str,
    target: SearchParamsTarget,
    fetch_limit: usize,
    results: &mut Vec<SearchHit>,
) -> Result<(), ReadError> {
    if matches!(target, SearchParamsTarget::All | SearchParamsTarget::Symbol) {
        for matched in index
            .symbols(query, fetch_limit)
            .map_err(ReadFault::index)?
        {
            if !includes(matcher, root, matched.file.path()) {
                continue;
            }
            results.push(symbol_search_hit(matched));
            if results.len() >= fetch_limit {
                return Ok(());
            }
        }
    }
    if results.len() < fetch_limit
        && matches!(target, SearchParamsTarget::All | SearchParamsTarget::File)
    {
        for (file, line, text) in index
            .source_matches(query, fetch_limit)
            .map_err(ReadFault::index)?
        {
            if !includes(matcher, root, file.path()) {
                continue;
            }
            results.push(file_search_hit(file, line, text));
            if results.len() >= fetch_limit {
                break;
            }
        }
    }
    Ok(())
}

/// Reaches files `selector.force_include` names outside the index - bypassing `[source]`
/// policy and `.gitignore`, never the hard floor - parses them on demand, and searches them
/// with the same scoring pipeline as indexed hits, appending up to `limit` total results.
/// Bounded to [`FORCE_INCLUDE_FILES_MAX`] matched files; a crossed bound refuses the search.
fn collect_force_include_hits(
    index: &WorkspaceIndex,
    selector: &PathSelector,
    query: &str,
    target: SearchParamsTarget,
    limit: usize,
    results: &mut Vec<SearchHit>,
) -> Result<(), ReadError> {
    if selector.force_include.is_empty() {
        return Ok(());
    }
    let extra = index
        .force_include_files(
            &pattern_strings(&selector.force_include),
            FORCE_INCLUDE_FILES_MAX,
        )
        .map_err(ReadFault::index)?;
    if matches!(target, SearchParamsTarget::All | SearchParamsTarget::Symbol) {
        for matched in rift_index::symbol_matches(&extra, query, limit - results.len()) {
            results.push(symbol_search_hit(matched));
        }
    }
    if results.len() < limit && matches!(target, SearchParamsTarget::All | SearchParamsTarget::File)
    {
        for (file, line, text) in
            rift_index::source_line_matches(&extra, query, limit - results.len())
        {
            results.push(file_search_hit(file, line, text));
        }
    }
    Ok(())
}

fn symbol_search_hit(matched: SymbolMatch<'_>) -> SearchHit {
    let score = symbol_match_score(matched.rank);
    build_symbol_hit(matched, score, vec![MatchedField::Name])
}

/// Builds one symbol hit's wire shape. `symbol_search_hit` and the lexical merge's
/// `lexical_symbol_hit` share this: both surface the same declaration, differing only in
/// score and which indexed field produced the match.
fn build_symbol_hit(
    matched: SymbolMatch<'_>,
    score: f64,
    matched_by: Vec<MatchedField>,
) -> SearchHit {
    SearchHit {
        hit: SearchHitTarget::Symbol {
            symbol: wire_symbol(matched),
        },
        score,
        matched_by,
        relationships: None,
        source: Some(excerpt(matched.file, matched.symbol.range).text),
        diagnostics: None,
        span: Some(source_span(matched.file.path(), matched.symbol.range)),
        line: Some(line::line_number_at(
            matched.file.source(),
            matched.symbol.range.start,
        )),
        path: Some(project_path(matched.file.path())),
        traversal_path: None,
        distance: None,
    }
}

fn file_search_hit(file: &IndexedFile, line_index: usize, text: String) -> SearchHit {
    let start = line::line_start_offset(file.source(), line_index);
    let end = start.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
    let range = ByteRange { start, end };
    SearchHit {
        hit: SearchHitTarget::File {
            file: File {
                id: file_id(file.path()),
                content: FileContent::Regular {
                    size: u64::try_from(file.source().len()).unwrap_or(u64::MAX),
                    executable: false,
                },
                languages: vec![rust_language()],
                regions: Vec::new(),
                semantic: true,
            },
        },
        score: 1.0,
        matched_by: vec![MatchedField::Content],
        relationships: None,
        source: Some(text),
        diagnostics: None,
        span: Some(source_span(file.path(), range)),
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
        SymbolMatchRank::QualifiedSuffix => 0.8,
        SymbolMatchRank::Substring => 0.7,
    }
}

/// Maps lexical `bm25`'s negative best-first rank into a `(0, 1)` score, monotone and
/// comparable within one request. A later dense-ranking layer fuses upstream of this merge,
/// so the mapping stays deliberately local instead of trying to anticipate that fusion.
fn lexical_score(rank: f64) -> f64 {
    let magnitude = (-rank).max(0.0);
    magnitude / (1.0 + magnitude)
}

/// Merges lexical search-index matches into `results`: a resolved symbol becomes a hit
/// scored by [`lexical_score`], and a text file's best-ranked chunk becomes a file hit whose
/// line is the first line containing a query term. Each merges against an
/// identifier-matched hit already in `results` by identity, rather than duplicating it.
fn collect_lexical_hits(
    index: &WorkspaceIndex,
    query: &str,
    target: SearchParamsTarget,
    lexical_matches: &[LexicalMatch],
    results: &mut Vec<SearchHit>,
) {
    if matches!(target, SearchParamsTarget::All | SearchParamsTarget::Symbol) {
        for matched in lexical_matches
            .iter()
            .filter(|matched| matched.kind() == LexicalUnitKind::Symbol)
        {
            let Some((file, symbol)) = resolve_symbol(index, matched.path(), matched.identity())
            else {
                // The lexical tier indexed this symbol at a tree revision the request's
                // revision guard already proved current, but a symbol it named can still be
                // gone from this exact index (a race the guard narrows, never closes to
                // zero); skipping it silently is correct, not a defect to surface.
                continue;
            };
            merge_symbol_hit(results, file, symbol, lexical_score(matched.rank()));
        }
    }
    if matches!(target, SearchParamsTarget::All | SearchParamsTarget::File) {
        for (path, rank) in best_rank_per_text_file(lexical_matches) {
            let Some(file) = index.text_files().iter().find(|file| file.path() == &path) else {
                continue;
            };
            let (line_number, range, text) = locate_query_line(file.content(), query);
            merge_file_hit(results, file, line_number, range, text, lexical_score(rank));
        }
    }
}

/// Resolves one lexical symbol match's identity back to its declaration in `index`. `path`
/// narrows the search to the one file the lexical unit named, so this stays a scan of that
/// file's own symbols rather than the whole index.
fn resolve_symbol<'a>(
    index: &'a WorkspaceIndex,
    path: &ProjectPath,
    identity: &str,
) -> Option<(&'a IndexedFile, &'a RustSymbol)> {
    let file = index.file(path)?;
    let symbol = file.syntax().symbols().iter().find(|symbol| {
        rift_core::rust_symbol_identity(file.path().as_str(), &symbol.qualified_name) == identity
    })?;
    Some((file, symbol))
}

/// Collapses text-file lexical matches to one entry per path, keeping the best (most
/// negative) `bm25` rank when a file contributed more than one chunk match.
fn best_rank_per_text_file(lexical_matches: &[LexicalMatch]) -> Vec<(ProjectPath, f64)> {
    let mut best: Vec<(ProjectPath, f64)> = Vec::new();
    for matched in lexical_matches
        .iter()
        .filter(|matched| matched.kind() == LexicalUnitKind::TextFile)
    {
        if let Some(entry) = best.iter_mut().find(|(path, _)| path == matched.path()) {
            entry.1 = entry.1.min(matched.rank());
        } else {
            best.push((matched.path().clone(), matched.rank()));
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

/// Merges one resolved lexical symbol hit: an identifier-matched hit for the same symbol
/// keeps its place and absorbs `score`; otherwise the lexical hit joins `results` new.
fn merge_symbol_hit(
    results: &mut Vec<SearchHit>,
    file: &IndexedFile,
    symbol: &RustSymbol,
    score: f64,
) {
    let identity = SymbolId(rift_core::rust_symbol_identity(
        file.path().as_str(),
        &symbol.qualified_name,
    ));
    let existing = results.iter_mut().find(
        |hit| matches!(&hit.hit, SearchHitTarget::Symbol { symbol } if symbol.id == identity),
    );
    if let Some(existing) = existing {
        absorb_content_match(existing, score);
        return;
    }
    let matched = SymbolMatch {
        file,
        symbol,
        // Unused: `lexical_symbol_hit`'s caller supplies `score` directly, never
        // `symbol_match_score`, so no identifier rank need be a genuine one here.
        rank: SymbolMatchRank::Substring,
    };
    results.push(build_symbol_hit(
        matched,
        score,
        vec![MatchedField::Content],
    ));
}

/// Merges one lexical text-file hit: an identifier-matched hit at the same path and line
/// keeps its place and absorbs `score`; otherwise the lexical hit joins `results` new.
fn merge_file_hit(
    results: &mut Vec<SearchHit>,
    file: &TextSourceFile,
    line_number: u64,
    range: ByteRange,
    text: String,
    score: f64,
) {
    let path = project_path(file.path());
    let existing = results.iter_mut().find(|hit| {
        matches!(&hit.hit, SearchHitTarget::File { .. })
            && hit.path.as_ref() == Some(&path)
            && hit.line == Some(line_number)
    });
    if let Some(existing) = existing {
        absorb_content_match(existing, score);
        return;
    }
    results.push(lexical_file_hit(file, line_number, range, text, score));
}

/// Records that `existing` also matched by content: adds [`MatchedField::Content`] when
/// absent, and raises its score to the better of the two.
fn absorb_content_match(existing: &mut SearchHit, score: f64) {
    if !existing.matched_by.contains(&MatchedField::Content) {
        existing.matched_by.push(MatchedField::Content);
    }
    existing.score = existing.score.max(score);
}

fn lexical_file_hit(
    file: &TextSourceFile,
    line_number: u64,
    range: ByteRange,
    text: String,
    score: f64,
) -> SearchHit {
    SearchHit {
        hit: SearchHitTarget::File {
            file: File {
                id: file_id(file.path()),
                content: FileContent::Regular {
                    size: u64::try_from(file.content().len()).unwrap_or(u64::MAX),
                    executable: false,
                },
                languages: Vec::new(),
                regions: Vec::new(),
                semantic: false,
            },
        },
        score,
        matched_by: vec![MatchedField::Content],
        relationships: None,
        source: Some(text),
        diagnostics: None,
        span: Some(source_span(file.path(), range)),
        line: Some(line_number),
        path: Some(project_path(file.path())),
        traversal_path: None,
        distance: None,
    }
}

/// Sorts merged hits by descending score, breaking ties on a hit's own stable wire identity
/// so the order does not depend on the arrival order of lexical matches, which carries no
/// guaranteed order of its own.
fn order_by_relevance(results: &mut [SearchHit]) {
    results.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| hit_identity(left).cmp(hit_identity(right)))
    });
}

/// The wire id `order_by_relevance` breaks score ties on.
fn hit_identity(hit: &SearchHit) -> &str {
    match &hit.hit {
        SearchHitTarget::Symbol { symbol } => symbol.id.0.as_str(),
        SearchHitTarget::File { file } => file.id.0.as_str(),
        SearchHitTarget::Node { node } => node.id.0.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use rift_core::SourceVisibility;
    use rift_core::constants::READ_RESULTS_MAX_DEFAULT;
    use rift_index::{LexicalMatch, LexicalUnitKind, WorkspaceIndexLimits};
    use rift_protocol::read::{
        ExactKind, Extensions, FileContent, FileId, MatchedField, Node, NodeId, SearchParams,
        SearchParamsTarget, TextRange,
    };
    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        ByteRange, File, ReadFault, ReadService, SearchHit, SearchHitTarget, SymbolMatchRank,
        rust_language, symbol_match_score,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub struct Beacon;\nimpl Beacon { pub fn signal(&self) {} }\n",
        )?;
        fs::write(directory.path().join("README.md"), "Beacon docs")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
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
        )?;
        Ok((directory, service))
    }

    #[test]
    fn search_combines_symbol_and_source_matches_with_limit() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "limit": 3
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;

        assert_eq!(results.len(), 3);
        // "Beacon" ties three ways at rank/score: the struct symbol and its own declaration
        // line both score 1.0, so the identity tie-break (`rift://file/...` sorts before
        // `rift://symbol/...`) puts the file hit first; the `signal` method's substring
        // match on the qualified name `Beacon::signal` scores lower and sorts last.
        assert_eq!(results[0]["hit"]["target"], "file");
        assert_eq!(results[0]["score"], 1.0);
        assert_eq!(results[0]["path"], json!("src/lib.rs"));
        assert_eq!(results[1]["hit"]["target"], "symbol");
        assert_eq!(results[1]["score"], 1.0);
        assert_eq!(results[2]["hit"]["target"], "symbol");
        assert!(
            results[2]["path"]
                .as_str()
                .is_some_and(|path| !path.is_empty()),
            "every hit must carry a non-empty project-relative path: {:#?}",
            results[2]
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
    /// text only, and the span a caller needs is the one already on the hit.
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
        assert_eq!(hit["span"]["range"]["start"], 0);
        Ok(())
    }

    /// A search answer served in full claims complete coverage over the request.
    #[test]
    fn search_result_coverage_claims_complete_for_the_request() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({"query": "Beacon"}))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        assert_eq!(value["coverage"]["state"], json!("complete"));
        assert_eq!(
            value["coverage"]["scope"],
            json!({"kind": "reach", "reach": "request"})
        );
        Ok(())
    }

    #[test]
    fn symbol_scores_preserve_semantic_rank() {
        let scores = [
            symbol_match_score(SymbolMatchRank::QualifiedExact),
            symbol_match_score(SymbolMatchRank::NameExact),
            symbol_match_score(SymbolMatchRank::QualifiedSuffix),
            symbol_match_score(SymbolMatchRank::Substring),
        ];
        assert!(scores.windows(2).all(|pair| pair[0] > pair[1]));
    }

    #[test]
    fn search_rejects_cursor_and_node_target() -> TestResult {
        let (_directory, service) = fixture()?;
        let cursor: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "cursor": "page-two"}))?;
        assert!(matches!(
            service
                .search(&cursor, &[])
                .expect_err("cursor reads must fail")
                .fault(),
            ReadFault::Unsupported { .. }
        ));

        let node_target: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "target": "node"}))?;
        assert!(matches!(
            service
                .search(&node_target, &[])
                .expect_err("node target must fail")
                .fault(),
            ReadFault::Unsupported { .. }
        ));
        Ok(())
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

    #[test]
    fn search_target_file_propagates_oversized_result_limit() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "target": "file",
            "limit": READ_RESULTS_MAX_DEFAULT as u64 + 1
        }))?;
        assert!(matches!(
            service
                .search(&params, &[])
                .expect_err("oversized file-target limit must fail")
                .fault(),
            ReadFault::Index(_)
        ));
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
            "target": "file"
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 1);
        assert!(results[0]["line"].as_u64().is_some_and(|line| line > 1));
        assert_eq!(results[0]["source"], "    // lookout marker");
        Ok(())
    }

    #[test]
    fn search_resolves_every_rust_symbol_kind_and_visibility() -> TestResult {
        let (_directory, service) = rich_fixture()?;
        let cases = [
            ("Level", "rust.enum", None),
            ("Speaks", "rust.trait", None),
            ("Alias", "rust.type_alias", None),
            ("MAX", "rust.constant", None),
            ("NAME", "rust.static", None),
            ("inner", "rust.module", None),
            ("noop", "rust.macro", None),
            ("Hidden", "rust.struct", Some("private")),
            ("scoped", "rust.function", Some("pub(crate)")),
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
                        .ok_or("facets must be array")?
                        .contains(&json!("public")),
                    "{name} must not carry public facet"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn search_filter_and_traversal_stay_refused_alone_and_with_paths() -> TestResult {
        let (_directory, service) = fixture()?;
        let filter = json!({
            "kind": "field",
            "field": {"field": "name", "op": "eq", "value": "Beacon"}
        });
        let traversal = json!({
            "seed": "rift://symbol/rust/src/lib.rs/Beacon",
            "intent": "trace"
        });
        let paths = json!({"include": ["src/lib.rs"]});
        let cases = [
            json!({"query": "Beacon", "filter": filter}),
            json!({"query": "Beacon", "traversal": traversal}),
            json!({"query": "Beacon", "filter": filter, "paths": paths}),
            json!({"query": "Beacon", "traversal": traversal, "paths": paths}),
        ];
        for case in cases {
            let params: SearchParams = serde_json::from_value(case.clone())?;
            assert!(
                matches!(
                    service
                        .search(&params, &[])
                        .expect_err("filter and traversal must stay refused")
                        .fault(),
                    ReadFault::Unsupported { .. }
                ),
                "case must refuse as unsupported: {case}"
            );
        }
        Ok(())
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
                .or_else(|| hit["hit"]["file"]["id"].as_str())
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
        assert!(
            results
                .iter()
                .all(|hit| hit["hit"]["file"]["id"] == json!("rift://file/src/lib.rs"))
        );
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
        // Cursor-based pagination is not served yet: `validate_common` refuses any request
        // that supplies one. Stability is proven the way the implementation guarantees it
        // instead - `paths` filtering happens before the page is cut to `limit`, so the top
        // hit of a `paths`-scoped search does not move as `limit` grows.
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
            assert!(hit["span"]["range"]["start"].is_u64());
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
            "paths": {"force_include": ["gitignored.rs"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &[])?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 1);
        let hit = &results[0];
        assert_eq!(hit["matched_by"][0], "content");
        let id = hit["hit"]["file"]["id"]
            .as_str()
            .ok_or("content hit must carry a file id")?;
        assert!(id.contains("gitignored.rs"));
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
        assert_eq!(
            value["warnings"],
            json!([]),
            "a search served from one index warns nothing"
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
            ReadFault::Unsupported {
                capability: "force_include at a revision"
            }
        ));
        Ok(())
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "lexical_score is deterministic pure arithmetic over exact literals; the \
                  equality it proves is exact, not a measurement subject to rounding drift"
    )]
    fn lexical_score_stays_within_zero_one_and_is_monotone_in_rank() {
        let ranks = [-100.0, -10.0, -5.0, -1.0, -0.5, -0.001];
        let scores: Vec<f64> = ranks
            .iter()
            .map(|&rank| super::lexical_score(rank))
            .collect();
        for (rank, score) in ranks.iter().zip(&scores) {
            assert!(
                *score > 0.0 && *score < 1.0,
                "score for rank {rank} must lie in (0, 1): {score}"
            );
        }
        for pair in scores.windows(2) {
            assert!(
                pair[0] > pair[1],
                "a more negative bm25 rank must score strictly higher: {pair:?}"
            );
        }
        // bm25 never produces a non-negative rank, but the mapping still clamps rather than
        // producing a negative or out-of-range score.
        assert_eq!(super::lexical_score(0.0), 0.0);
        assert_eq!(super::lexical_score(5.0), 0.0);
    }

    fn indexed_symbol<'a>(
        service: &'a ReadService,
        path: &str,
        name: &str,
    ) -> TestResult<(&'a rift_index::IndexedFile, &'a rift_syntax::RustSymbol)> {
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
    #[expect(
        clippy::float_cmp,
        reason = "the score under test is `f64::max` over exact literals, not a measurement \
                  subject to rounding drift"
    )]
    fn merge_symbol_hit_unions_matched_by_and_keeps_the_higher_score() -> TestResult {
        let (_directory, service) = fixture()?;
        let (file, symbol) = indexed_symbol(&service, "src/lib.rs", "Beacon")?;
        let existing = super::build_symbol_hit(
            super::SymbolMatch {
                file,
                symbol,
                rank: SymbolMatchRank::NameExact,
            },
            0.5,
            vec![MatchedField::Name],
        );
        let mut results = vec![existing];

        super::merge_symbol_hit(&mut results, file, symbol, 0.9);
        assert_eq!(results.len(), 1, "the same symbol must not duplicate");
        assert_eq!(results[0].score, 0.9, "the higher score must win");
        assert_eq!(
            results[0].matched_by,
            vec![MatchedField::Name, MatchedField::Content]
        );

        // A second merge at a lower score keeps the existing higher score and does not
        // duplicate the already-present Content field.
        super::merge_symbol_hit(&mut results, file, symbol, 0.1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score, 0.9);
        assert_eq!(
            results[0].matched_by,
            vec![MatchedField::Name, MatchedField::Content]
        );
        Ok(())
    }

    #[test]
    fn collect_lexical_hits_skips_a_symbol_identity_the_index_no_longer_carries() -> TestResult {
        let (_directory, service) = fixture()?;
        let path = rift_core::ProjectPath::new("src/lib.rs")?;
        let vanished = LexicalMatch::new(
            "rift://symbol/rust/src/lib.rs/Vanished",
            path,
            LexicalUnitKind::Symbol,
            Some("Vanished".to_owned()),
            -5.0,
        );
        let mut results = Vec::new();
        super::collect_lexical_hits(
            service.index(),
            "Beacon",
            SearchParamsTarget::All,
            &[vanished],
            &mut results,
        );
        assert!(
            results.is_empty(),
            "a symbol identity absent from the index must be skipped silently: {results:#?}"
        );
        Ok(())
    }

    #[test]
    fn collect_lexical_hits_skips_a_text_file_path_the_index_no_longer_carries() -> TestResult {
        // `fixture()` includes `README.md` as its only text file, so `guide.md` is a path the
        // index never carried.
        let (_directory, service) = fixture()?;
        let vanished = LexicalMatch::new(
            "guide.md",
            rift_core::ProjectPath::new("guide.md")?,
            LexicalUnitKind::TextFile,
            Some("guide".to_owned()),
            -5.0,
        );
        let mut results = Vec::new();
        super::collect_lexical_hits(
            service.index(),
            "Beacon",
            SearchParamsTarget::All,
            &[vanished],
            &mut results,
        );
        assert!(
            results.is_empty(),
            "a text-file path absent from the index must be skipped silently: {results:#?}"
        );
        Ok(())
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the winning score is `lexical_score` applied to the exact literal rank the \
                  test supplies, not a measurement subject to rounding drift"
    )]
    fn text_file_lexical_matches_dedupe_to_one_hit_at_the_best_rank() -> TestResult {
        let directory = tempfile::tempdir()?;
        let content = "intro\nsearch replace all units here\nend\n";
        fs::write(directory.path().join("guide.md"), content)?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text_inclusion = rift_core::TextFileInclusion::default();
        let service = ReadService::build(directory.path(), limits, &visibility, &text_inclusion)?;
        let worse = LexicalMatch::new(
            "guide.md#0",
            rift_core::ProjectPath::new("guide.md")?,
            LexicalUnitKind::TextFile,
            Some("guide".to_owned()),
            -1.0,
        );
        let better = LexicalMatch::new(
            "guide.md#1",
            rift_core::ProjectPath::new("guide.md")?,
            LexicalUnitKind::TextFile,
            Some("guide".to_owned()),
            -9.0,
        );
        let mut results = Vec::new();
        super::collect_lexical_hits(
            service.index(),
            "units",
            SearchParamsTarget::All,
            &[worse, better],
            &mut results,
        );
        assert_eq!(
            results.len(),
            1,
            "chunk matches for the same file must collapse to one hit: {results:#?}"
        );
        assert_eq!(results[0].score, super::lexical_score(-9.0));
        Ok(())
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the winning score is an exact literal the test supplies, not a measurement \
                  subject to rounding drift"
    )]
    fn merge_file_hit_absorbs_a_second_match_at_the_same_path_and_line() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("guide.md"), "alpha units beta\n")?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text_inclusion = rift_core::TextFileInclusion::default();
        let service = ReadService::build(directory.path(), limits, &visibility, &text_inclusion)?;
        let file = service
            .index()
            .text_files()
            .first()
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
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score, 0.4);
        assert_eq!(results[0].matched_by, vec![MatchedField::Content]);

        super::merge_file_hit(
            &mut results,
            file,
            1,
            range,
            "alpha units beta".to_owned(),
            0.9,
        );
        assert_eq!(
            results.len(),
            1,
            "a second match at the same path and line must not duplicate the hit"
        );
        assert_eq!(results[0].score, 0.9, "the higher score must win");
        assert_eq!(
            results[0].matched_by,
            vec![MatchedField::Content],
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
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score, 0.9);
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

    fn file_hit_stub(id: &str, score: f64) -> SearchHit {
        SearchHit {
            hit: SearchHitTarget::File {
                file: File {
                    id: FileId(id.to_owned()),
                    content: FileContent::Regular {
                        size: 0,
                        executable: false,
                    },
                    languages: Vec::new(),
                    regions: Vec::new(),
                    semantic: false,
                },
            },
            score,
            matched_by: vec![MatchedField::Content],
            relationships: None,
            source: None,
            diagnostics: None,
            span: None,
            line: None,
            path: None,
            traversal_path: None,
            distance: None,
        }
    }

    #[test]
    fn order_by_relevance_sorts_by_score_then_a_deterministic_identity_tie_break() {
        let mut first_arrival = vec![
            file_hit_stub("rift://file/z.rs", 0.5),
            file_hit_stub("rift://file/b.rs", 0.9),
            file_hit_stub("rift://file/a.rs", 0.9),
        ];
        super::order_by_relevance(&mut first_arrival);
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
        super::order_by_relevance(&mut second_arrival);
        let reordered_ids: Vec<&str> = second_arrival.iter().map(super::hit_identity).collect();
        assert_eq!(reordered_ids, ordered_ids);
    }

    fn node_hit_stub(id: &str, score: f64) -> SearchHit {
        SearchHit {
            hit: SearchHitTarget::Node {
                node: Node {
                    id: NodeId(id.to_owned()),
                    symbol: None,
                    unit: FileId("rift://file/lib.rs".to_owned()),
                    language: rust_language(),
                    kind: ExactKind("rust.function_item".to_owned()),
                    facets: Vec::new(),
                    range: TextRange { start: 0, end: 1 },
                    regions: Vec::new(),
                    parent: None,
                    extensions: Extensions(std::collections::BTreeMap::new()),
                },
            },
            score,
            matched_by: vec![MatchedField::Content],
            relationships: None,
            source: None,
            diagnostics: None,
            span: None,
            line: None,
            path: None,
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
        super::order_by_relevance(&mut results);
        let ids: Vec<&str> = results.iter().map(super::hit_identity).collect();
        // "rift://file/..." sorts before "rift://node/..." lexicographically ('f' < 'n').
        assert_eq!(
            ids,
            ["rift://file/z.rs", "rift://node/rust/lib.rs@0-1#00000000",]
        );
    }
}
