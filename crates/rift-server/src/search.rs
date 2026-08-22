//! `search` tool execution: lexical symbol and source-line search, narrowed or extended by a
//! request's `paths` selector. Extracted from `read` so that module stays below its size bound.

use std::path::Path;

use rift_core::ProjectPath;
use rift_core::constants::{FORCE_INCLUDE_FILES_MAX, SEARCH_RESULTS_DEFAULT};
use rift_core::line;
use rift_index::{PathMatcher, SymbolMatch, SymbolMatchRank, WorkspaceIndex};
use rift_protocol::read::{
    File, FileContent, MatchedField, PathPattern, PathSelector, SearchHit, SearchHitTarget,
    SearchParams, SearchParamsTarget, SearchResult,
};
use rift_syntax::ByteRange;

use crate::read::{
    ReadError, ReadFault, ReadService, admitted_limit, complete_coverage, excerpt, file_id,
    project_path, rust_language, source_span, validate_common, wire_symbol,
};

impl ReadService {
    /// Searches Rust declarations and source lines, optionally narrowed or extended by
    /// `params.paths`.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for deferred filters, traversal, cursors, projections, scopes, an
    /// invalid `paths` glob, or a `force_include` bound crossed.
    pub fn search(&self, params: &SearchParams) -> Result<SearchResult, ReadError> {
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
        let limit = admitted_limit(params.limit.unwrap_or(SEARCH_RESULTS_DEFAULT as u64))?;
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
            limit,
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
        results.truncate(limit);
        let snapshot = self.snapshot().clone();
        Ok(SearchResult {
            coverage: complete_coverage(&snapshot),
            snapshot,
            results,
            next_cursor: None,
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

/// Whether `path` (project-relative) passes `matcher`, absent a matcher admitting every path.
fn admits(matcher: Option<&PathMatcher>, root: &Path, path: &ProjectPath) -> bool {
    matcher.is_none_or(|matcher| matcher.admits(&root.join(path.as_str())))
}

/// Symbol and lexical hits from the index, filtered by `matcher` before the page is cut down to
/// `limit` so a `paths`-narrowed search still returns a full page where enough matches exist.
#[expect(
    clippy::too_many_arguments,
    reason = "each parameter is a distinct search input; grouping them would just move the count into a struct only this function reads"
)]
fn collect_indexed_hits(
    index: &WorkspaceIndex,
    matcher: Option<&PathMatcher>,
    root: &Path,
    query: &str,
    target: SearchParamsTarget,
    fetch_limit: usize,
    limit: usize,
    results: &mut Vec<SearchHit>,
) -> Result<(), ReadError> {
    if matches!(target, SearchParamsTarget::All | SearchParamsTarget::Symbol) {
        for matched in index
            .symbols(query, fetch_limit)
            .map_err(ReadFault::index)?
        {
            if !admits(matcher, root, matched.file.path()) {
                continue;
            }
            results.push(symbol_search_hit(matched));
            if results.len() >= limit {
                return Ok(());
            }
        }
    }
    if results.len() < limit && matches!(target, SearchParamsTarget::All | SearchParamsTarget::File)
    {
        for (file, line, text) in index
            .source_matches(query, fetch_limit)
            .map_err(ReadFault::index)?
        {
            if !admits(matcher, root, file.path()) {
                continue;
            }
            results.push(file_search_hit(file, line, text));
            if results.len() >= limit {
                break;
            }
        }
    }
    Ok(())
}

/// Reaches files `selector.force_include` names outside the index — bypassing `[source]`
/// policy and `.gitignore`, never the hard floor — parses them on demand, and searches them
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
    SearchHit {
        hit: SearchHitTarget::Symbol {
            symbol: wire_symbol(matched),
        },
        score,
        matched_by: vec![MatchedField::Name],
        relationships: None,
        source: Some(excerpt(matched.file, matched.symbol.range).text),
        diagnostics: None,
        span: Some(source_span(matched.file, matched.symbol.range)),
        line: Some(line::line_number_at(
            matched.file.source(),
            matched.symbol.range.start,
        )),
        path: Some(project_path(matched.file)),
        traversal_path: None,
        distance: None,
    }
}

fn file_search_hit(file: &rift_index::IndexedFile, line_index: usize, text: String) -> SearchHit {
    let start = line::line_start_offset(file.source(), line_index);
    let end = start.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
    let range = ByteRange { start, end };
    SearchHit {
        hit: SearchHitTarget::File {
            file: File {
                id: file_id(file),
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
        span: Some(source_span(file, range)),
        line: Some(u64::try_from(line_index).unwrap_or(u64::MAX)),
        path: Some(project_path(file)),
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

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use rift_core::SourceVisibility;
    use rift_core::constants::READ_RESULTS_MAX_DEFAULT;
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::read::SearchParams;
    use serde_json::json;
    use tempfile::TempDir;

    use super::{ReadFault, ReadService, SymbolMatchRank, symbol_match_score};

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
            &rift_core::TextFileAdmission::default(),
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
            &rift_core::TextFileAdmission::default(),
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
            &rift_core::TextFileAdmission::default(),
        )?;
        Ok((directory, service))
    }

    /// One indexed file, one hidden by `.gitignore`, one hidden by `[source].exclude`, and one
    /// under the hard floor (`.git/`) — the fixture `force_include` tests reach into.
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
            &rift_core::TextFileAdmission::default(),
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
        let value = serde_json::to_value(service.search(&params)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["hit"]["target"], "symbol");
        assert_eq!(results[0]["score"], 1.0);
        assert_eq!(results[0]["path"], json!("src/lib.rs"));
        assert_eq!(results[2]["hit"]["target"], "file");
        assert_eq!(results[2]["line"], 1);
        assert!(
            results[2]["path"]
                .as_str()
                .is_some_and(|path| !path.is_empty()),
            "the file hit must carry a non-empty project-relative path: {:#?}",
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
        let hit = &value["results"][0];
        assert!(
            hit["source"].is_string(),
            "excerpt must serialize as a bare string, not an object carrying a span: {hit}"
        );
        assert_eq!(hit["source"], json!("pub struct Beacon;"));
        assert_eq!(hit["span"]["range"]["start"], 0);
        Ok(())
    }

    /// `complete_coverage` used to fill every origin revision with an all-zero digest; the
    /// search answer now carries the snapshot's real revisions.
    #[test]
    fn search_result_origins_carry_the_snapshots_real_revisions() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({"query": "Beacon"}))?;
        let value = serde_json::to_value(service.search(&params)?)?;
        let snapshot_tree = value["snapshot"]["tree_revision"].clone();
        let snapshot_source = value["snapshot"]["source_revision"].clone();
        let origin = &value["coverage"]["origins"][0];
        assert_eq!(origin["tree_revision"], snapshot_tree);
        assert_eq!(origin["source_revision"], snapshot_source);
        assert_ne!(origin["tree_revision"], json!("00000000"));
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
                .search(&cursor)
                .expect_err("cursor reads must fail")
                .fault(),
            ReadFault::Unsupported { .. }
        ));

        let node_target: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "target": "node"}))?;
        assert!(matches!(
            service
                .search(&node_target)
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
                .search(&missing_query)
                .expect_err("missing query must fail")
                .fault(),
            ReadFault::Invalid { .. }
        ));

        let empty_query: SearchParams = serde_json::from_value(json!({"query": ""}))?;
        assert!(matches!(
            service
                .search(&empty_query)
                .expect_err("empty query must fail")
                .fault(),
            ReadFault::Invalid { .. }
        ));

        let zero_limit: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "limit": 0}))?;
        let error = service
            .search(&zero_limit)
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
                .search(&params)
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
            let value = serde_json::to_value(service.search(&params)?)?;
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
                        .search(&params)
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
        let result = service.search(&params)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
                .search(&params)
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
                .search(&params)
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
        // instead — `paths` filtering happens before the page is cut to `limit`, so the top
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
        let narrow_value = serde_json::to_value(service.search(&narrow)?)?;
        let wide_value = serde_json::to_value(service.search(&wide)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
        let value = serde_json::to_value(service.search(&params)?)?;
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
            &rift_core::TextFileAdmission::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "extra",
            "paths": {"force_include": ["extra_*.rs"]}
        }))?;
        assert!(matches!(
            service
                .search(&params)
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
                .search(&params)
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
        let value = serde_json::to_value(service.search(&committed)?)?;
        let results = value["results"].as_array().ok_or("results array")?;
        assert!(!results.is_empty(), "the committed declaration matches");
        assert!(
            value["snapshot"]["revision"].as_str().is_some(),
            "a revision search's snapshot carries the resolved commit"
        );
        let drifted: SearchParams = serde_json::from_value(json!({"query": "drifted_probe"}))?;
        let drifted_value = serde_json::to_value(service.search(&drifted)?)?;
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
        let error = service.search(&params).expect_err(
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
}
