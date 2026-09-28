use std::error::Error;
use std::fmt::Write as _;
use std::fs;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{LexicalIndexLimits, WorkspaceIndexLimits};
use rift_protocol::configuration::{ByteSize, HistoryConfiguration, SearchConfiguration};
use rift_protocol::read::{
    MatchedField, ReadWarning, SEARCH_PATTERN_BYTES_MAX, SearchHitTarget, SearchParams,
    SearchResult,
};
use rift_search::{RevisionScoped, SearchIndex, SearchIndexLimits};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::{PatternBounds, StoreAnswer, accepted_pattern};
use crate::read::{ReadFault, ReadService};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Chunk bound small enough that `notes/long.txt` is stored as several rows.
const CHUNK_BYTES: u64 = 1_024;

/// A tree holding a parsed Rust file, a CRLF file, a multibyte file, and a text file past
/// the chunk bound whose one `NEEDLE` sits in a later chunk.
fn fixture() -> TestResult<(TempDir, ReadService)> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join("notes"))?;
    fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha() {\n    todo!()\n}\n// TODO: beta;\n",
    )?;
    fs::write(root.join("notes/crlf.txt"), "let a = 1;\r\nFIXME = 2;\r\n")?;
    fs::write(
        root.join("notes/grusse.txt"),
        "gr\u{fc}\u{df}e \u{65e5}\u{672c}\u{8a9e} TODO\nzeile 123\n",
    )?;
    let mut long = String::new();
    for line in 0..200 {
        writeln!(long, "filler line {line:03} of text")?;
    }
    long.push_str("the NEEDLE sits here\n");
    fs::write(root.join("notes/long.txt"), long)?;
    let service = service_over(root)?;
    Ok((directory, service))
}

fn service_over(root: &std::path::Path) -> TestResult<ReadService> {
    let include = TextFileInclusion::default().include().to_vec();
    Ok(ReadService::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::new(include, CHUNK_BYTES),
        HistoryConfiguration::default(),
    )?)
}

async fn published_store(directory: &TempDir, service: &ReadService) -> TestResult<SearchIndex> {
    let limits = SearchIndexLimits::builder(LexicalIndexLimits::default())
        .disable_vector()
        .build();
    let store = SearchIndex::open(&directory.path().join(".rift-test-db"), limits).await?;
    store
        .replace_lexical(&service.index_documents(), service.tree_revision())
        .await?;
    Ok(store)
}

fn params(request: Value) -> TestResult<SearchParams> {
    Ok(serde_json::from_value(request)?)
}

/// What the store selects for `request`'s pattern under `bounds`, as the answer the read
/// path takes.
async fn store_answer(
    store: &SearchIndex,
    service: &ReadService,
    request: &SearchParams,
    bounds: PatternBounds,
) -> TestResult<StoreAnswer> {
    let answer = StoreAnswer::identifier_only().with_pattern_bounds(bounds);
    let pattern = accepted_pattern(request, bounds)?.ok_or("the request names a pattern")?;
    let Some(prefilter) = pattern.prefilter() else {
        return Ok(answer);
    };
    let scoped = store
        .pattern_candidates(
            service.tree_revision(),
            prefilter,
            pattern.is_line_bound(),
            bounds.candidate_rows_max(),
        )
        .await?;
    let RevisionScoped::Matched(candidates) = scoped else {
        return Err("the store holds the revision it was just stamped with".into());
    };
    Ok(answer.with_pattern_candidates(candidates))
}

/// Each hit as path, line, range start, and range end.
fn located(result: &SearchResult) -> Vec<(String, u64, u64, u64)> {
    result
        .results
        .iter()
        .map(|hit| {
            let range = hit.range.as_ref().expect("a pattern hit carries its range");
            (
                hit.path.as_ref().expect("a hit carries its path").0.clone(),
                hit.line.expect("a pattern hit carries its line"),
                range.start,
                range.end,
            )
        })
        .collect()
}

/// Searches `request` through the trigram candidates, and checks the answer equals the
/// one a scan of every held file gives: the candidates lose no match.
async fn searched(
    directory: &TempDir,
    service: &ReadService,
    request: Value,
) -> TestResult<SearchResult> {
    let request = params(request)?;
    let store = published_store(directory, service).await?;
    let answer = store_answer(&store, service, &request, PatternBounds::default()).await?;
    let result = service.search(&request, &answer)?;
    let scanned = service.search(&request, &StoreAnswer::identifier_only())?;
    assert_eq!(
        located(&result),
        located(&scanned),
        "candidates dropped a match"
    );
    Ok(result)
}

#[tokio::test]
async fn an_alternation_answers_file_hits_with_line_and_range() -> TestResult {
    let (directory, service) = fixture()?;
    let result = searched(
        &directory,
        &service,
        json!({"pattern": "TODO|FIXME", "target": "file"}),
    )
    .await?;
    assert_eq!(
        located(&result),
        [
            ("notes/crlf.txt".to_owned(), 2, 12, 17),
            ("notes/grusse.txt".to_owned(), 1, 18, 22),
            ("src/lib.rs".to_owned(), 4, 34, 38),
        ]
    );
    let first = &result.results[0];
    assert_eq!(first.matched_by, [MatchedField::Content]);
    assert_eq!(first.score, None);
    assert_eq!(first.source, None, "no source was asked for");
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    Ok(())
}

#[tokio::test]
async fn case_insensitive_matching_is_the_inline_flag() -> TestResult {
    let (directory, service) = fixture()?;
    let result = searched(
        &directory,
        &service,
        json!({"pattern": "(?i)todo", "target": "file"}),
    )
    .await?;
    let lines: Vec<_> = located(&result)
        .into_iter()
        .map(|(path, line, ..)| (path, line))
        .collect();
    assert_eq!(
        lines,
        [
            ("notes/grusse.txt".to_owned(), 1),
            ("src/lib.rs".to_owned(), 2),
            ("src/lib.rs".to_owned(), 4),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_crlf_line_ends_after_its_carriage_return_as_ripgrep_reads_it() -> TestResult {
    let (directory, service) = fixture()?;
    let semicolon_at_end = searched(&directory, &service, json!({"pattern": "2;$"})).await?;
    assert!(
        semicolon_at_end.results.is_empty(),
        "`$` sits before `\\n` alone"
    );
    let carriage = searched(&directory, &service, json!({"pattern": "2;\\r$"})).await?;
    assert_eq!(
        located(&carriage),
        [("notes/crlf.txt".to_owned(), 2, 20, 23)]
    );
    Ok(())
}

#[tokio::test]
async fn a_multibyte_line_answers_byte_offsets_and_its_line_as_source() -> TestResult {
    let (directory, service) = fixture()?;
    let result = searched(
        &directory,
        &service,
        json!({"pattern": "\u{65e5}\u{672c}\u{8a9e}", "include": ["source"]}),
    )
    .await?;
    assert_eq!(
        located(&result),
        [("notes/grusse.txt".to_owned(), 1, 8, 17)]
    );
    assert_eq!(
        result.results[0].source.as_deref(),
        Some("gr\u{fc}\u{df}e \u{65e5}\u{672c}\u{8a9e} TODO")
    );
    Ok(())
}

#[tokio::test]
async fn a_pattern_with_no_prefilter_scans_every_held_file() -> TestResult {
    let (_directory, service) = fixture()?;
    let request =
        params(json!({"pattern": "[0-9]{3}", "paths": {"include": ["notes/grusse.txt"]}}))?;
    let pattern = accepted_pattern(&request, PatternBounds::default())?.ok_or("names a pattern")?;
    assert!(pattern.prefilter().is_none());
    let result = service.search(&request, &StoreAnswer::identifier_only())?;
    assert_eq!(
        located(&result),
        [("notes/grusse.txt".to_owned(), 2, 29, 32)]
    );
    Ok(())
}

#[tokio::test]
async fn a_match_in_a_later_chunk_answers_its_offset_in_the_file() -> TestResult {
    let (directory, service) = fixture()?;
    let text = fs::read_to_string(directory.path().join("notes/long.txt"))?;
    assert!(
        text.len() as u64 > 3 * CHUNK_BYTES,
        "the file spans several chunks"
    );
    let start = text.find("NEEDLE").ok_or("the fixture holds the needle")? as u64;
    let store = published_store(&directory, &service).await?;
    let request = params(json!({"pattern": "NEEDLE"}))?;
    let answer = store_answer(&store, &service, &request, PatternBounds::default()).await?;
    let selected = answer
        .pattern_candidates()
        .ok_or("the store answered")?
        .candidates();
    assert_eq!(selected.len(), 1, "one file holds the needle");
    let [span] = selected[0].spans() else {
        return Err("one chunk row holds the needle".into());
    };
    assert!(span.start > 0 && span.start <= start && start < span.end);
    let result = service.search(&request, &answer)?;
    assert_eq!(
        located(&result),
        [("notes/long.txt".to_owned(), 201, start, start + 6)]
    );
    Ok(())
}

#[tokio::test]
async fn a_literal_line_feed_selects_whole_files_and_crosses_a_chunk() -> TestResult {
    let (directory, service) = fixture()?;
    let result = searched(
        &directory,
        &service,
        json!({"pattern": "041 of text\\nfiller line 042"}),
    )
    .await?;
    assert_eq!(located(&result).len(), 1);
    Ok(())
}

/// Each hit as its kind (a declaration's name, or `file`), path, and line.
fn kinds(result: &SearchResult) -> Vec<(String, String, u64)> {
    result
        .results
        .iter()
        .map(|hit| {
            let kind = match &hit.hit {
                SearchHitTarget::Symbol { symbol } => symbol.name.clone(),
                _ => "file".to_owned(),
            };
            let path = hit.path.as_ref().expect("a hit carries its path").0.clone();
            (kind, path, hit.line.expect("a hit carries its line"))
        })
        .collect()
}

#[tokio::test]
async fn target_symbol_answers_each_declaration_holding_a_match_once() -> TestResult {
    let (directory, service) = fixture()?;
    let symbols = searched(
        &directory,
        &service,
        json!({"pattern": "(?i)todo|alpha", "target": "symbol"}),
    )
    .await?;
    // `alpha` holds two matches; the `// TODO` comment sits outside every declaration.
    assert_eq!(
        kinds(&symbols),
        [("alpha".to_owned(), "src/lib.rs".to_owned(), 1)]
    );
    assert_eq!(symbols.results[0].matched_by, [MatchedField::Content]);
    assert_eq!(symbols.results[0].score, None);
    let both = searched(&directory, &service, json!({"pattern": "(?i)todo"})).await?;
    assert_eq!(
        kinds(&both),
        [
            ("file".to_owned(), "notes/grusse.txt".to_owned(), 1),
            ("alpha".to_owned(), "src/lib.rs".to_owned(), 1),
            ("file".to_owned(), "src/lib.rs".to_owned(), 2),
            ("file".to_owned(), "src/lib.rs".to_owned(), 4),
        ],
        "path order, then offset, a declaration at its first match"
    );
    let with_source = searched(
        &directory,
        &service,
        json!({"pattern": "todo!", "target": "symbol", "include": ["source"]}),
    )
    .await?;
    assert_eq!(
        with_source.results[0].source.as_deref(),
        Some("pub fn alpha() {\n    todo!()\n}")
    );
    Ok(())
}

#[tokio::test]
async fn orders_other_than_relevance_sort_the_hits() -> TestResult {
    let (_directory, service) = fixture()?;
    let request = params(json!({"pattern": "(?i)todo", "order": "path", "limit": 2}))?;
    let first = service.search(&request, &StoreAnswer::identifier_only())?;
    assert_eq!(first.pagination.total_pages, 2);
    assert_eq!(first.results.len(), 2);
    let request = params(json!({"pattern": "(?i)todo", "order": "identity"}))?;
    let identity = service.search(&request, &StoreAnswer::identifier_only())?;
    assert_eq!(identity.results.len(), 4);
    Ok(())
}

/// The refusal `request` meets, as its field and message.
fn refusal(service: &ReadService, request: Value) -> TestResult<String> {
    let error = service
        .search(&params(request)?, &StoreAnswer::identifier_only())
        .expect_err("the request is refused");
    let ReadFault::Invalid { field, .. } = error.fault() else {
        return Err(format!("expected invalid_request, found {error}").into());
    };
    assert_eq!(*field, "pattern", "{error}");
    Ok(error.to_string())
}

#[test]
fn refusals_name_the_field_and_the_bound() -> TestResult {
    let (_directory, service) = fixture()?;
    let too_long = "a".repeat(SEARCH_PATTERN_BYTES_MAX + 1);
    let cases = [
        (json!({"pattern": "useState("}), "unclosed group"),
        (json!({"pattern": too_long}), "exceeds the maximum 1024"),
        (json!({"pattern": ""}), "empty"),
        (
            json!({"pattern": "\\w{500}"}),
            "exceeds 1048576 bytes, the [search] pattern_compiled_size bound",
        ),
        (json!({"pattern": "TODO", "query": "todo"}), "`query`"),
        (
            json!({"pattern": "TODO", "traversal": {"seed": "rift://symbol/rust/src/lib.rs/alpha"}}),
            "a walk answers relationships",
        ),
        (
            json!({"pattern": "TODO", "change": {"base": "main"}}),
            "committed revisions",
        ),
        (
            json!({"pattern": "TODO", "rev": "main"}),
            "current tree alone",
        ),
        (json!({"pattern": "TODO", "scope": "all"}), "`scope`"),
    ];
    for (request, expected) in cases {
        let message = refusal(&service, request.clone())?;
        assert!(message.contains(expected), "{request}: {message}");
    }
    let documentation = service
        .search(
            &params(json!({"pattern": "TODO", "target": "documentation"}))?,
            &StoreAnswer::identifier_only(),
        )
        .expect_err("documentation blocks answer no pattern");
    assert!(matches!(
        documentation.fault(),
        ReadFault::Unsupported { .. }
    ));
    Ok(())
}

/// `packages` reaches the global API alone and `pattern` reads the project's trigram index
/// alone, so no scope answers both. The refusal names the first field the request cannot
/// answer, in the order every search is validated: `packages` beside the `local` scope or
/// a revision, and `pattern` beside a scope that reaches packages.
#[test]
fn a_pattern_beside_packages_refuses_naming_the_field_that_cannot_answer() -> TestResult {
    let (_directory, service) = fixture()?;
    let packages = json!([{"manager": "cargo", "name": "serde"}]);
    let cases = [
        (
            json!({"pattern": "TODO", "packages": packages}),
            "packages",
            "the local scope reads the project alone",
        ),
        (
            json!({"pattern": "TODO", "scope": "all", "packages": packages}),
            "pattern",
            "`scope`",
        ),
        (
            json!({"pattern": "TODO", "scope": "global", "packages": packages}),
            "pattern",
            "`scope`",
        ),
        (
            json!({"pattern": "TODO", "scope": "all", "rev": "main", "packages": packages}),
            "packages",
            "current tree alone",
        ),
    ];
    for (request, expected_field, expected) in cases {
        let error = service
            .search(&params(request.clone())?, &StoreAnswer::identifier_only())
            .expect_err("no scope answers both fields");
        let ReadFault::Invalid { field, .. } = error.fault() else {
            return Err(format!("expected invalid_request, found {error}").into());
        };
        assert_eq!(*field, expected_field, "{request}: {error}");
        let message = error.to_string();
        assert!(message.contains(expected), "{request}: {message}");
    }
    Ok(())
}

/// The bounds the default `[search]` table sets, with the one key `adjust` changes.
fn bounds_with(adjust: impl FnOnce(&mut SearchConfiguration)) -> PatternBounds {
    let mut search = SearchConfiguration::default();
    adjust(&mut search);
    PatternBounds::from(&search)
}

#[tokio::test]
async fn a_selection_past_the_candidate_rows_refuses_naming_the_bound() -> TestResult {
    let (directory, service) = fixture()?;
    let store = published_store(&directory, &service).await?;
    let request = params(json!({"pattern": "filler line"}))?;
    let bounds = bounds_with(|search| search.pattern_candidate_rows = 2);
    let answer = store_answer(&store, &service, &request, bounds).await?;
    assert_eq!(
        answer
            .pattern_candidates()
            .and_then(rift_index::PatternCandidates::truncated_at),
        Some(2),
        "every chunk of the long file holds the pattern"
    );
    let error = service
        .search(&request, &answer)
        .expect_err("a cut selection refuses");
    let message = error.to_string();
    assert!(
        message.contains("more than 2 candidate rows, the [search] pattern_candidate_rows bound"),
        "{message}"
    );
    Ok(())
}

#[test]
fn a_scan_past_the_verified_bytes_refuses_naming_the_bound() -> TestResult {
    let (_directory, service) = fixture()?;
    let bounds = bounds_with(|search| {
        search.pattern_verified_size = ByteSize::from_bytes(1 << 20);
    });
    let within = service.search(
        &params(json!({"pattern": "[0-9]{3}"}))?,
        &StoreAnswer::identifier_only().with_pattern_bounds(bounds),
    )?;
    assert!(!within.results.is_empty(), "the fixture fits the bound");

    let directory = tempfile::tempdir()?;
    let line = format!("{}\n", "x".repeat(1_000));
    fs::write(directory.path().join("big.txt"), line.repeat(1_100))?;
    let service = service_over(directory.path())?;
    let request = params(json!({"pattern": "y{3}"}))?;
    let error = service
        .search(
            &request,
            &StoreAnswer::identifier_only().with_pattern_bounds(bounds),
        )
        .expect_err("a scan past the bound refuses");
    assert!(
        error
            .to_string()
            .contains("more than 1048576 bytes to verify, the [search] pattern_verified_size"),
        "{error}"
    );
    Ok(())
}

#[test]
fn a_file_past_the_matches_bound_is_cut_and_warned_apart_from_the_result_bound() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("many.txt"), "x;\n".repeat(30))?;
    fs::write(directory.path().join("once.txt"), "x;\n")?;
    let service = service_over(directory.path())?;
    let bounds = bounds_with(|search| search.pattern_matches_per_file = 20);
    let request = params(json!({"pattern": "x;", "limit": 10}))?;
    let result = service.search(
        &request,
        &StoreAnswer::identifier_only().with_pattern_bounds(bounds),
    )?;
    assert_eq!(
        result.pagination.total_pages, 3,
        "20 cut matches and one whole"
    );
    assert_eq!(
        result.warnings,
        [ReadWarning::PatternMatchesTruncated {
            matches_per_file: 20,
            files: vec![rift_protocol::read::FileId(
                "rift://file/many.txt".to_owned()
            )],
        }],
        "the cut is its own warning, never results_truncated"
    );
    Ok(())
}

#[tokio::test]
async fn notebooks_split_lines_and_force_included_files_are_verified_whole() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(
        root.join("notes.ipynb"),
        r#"{"cells":[{"cell_type":"code","source":["print('beacon')"],"metadata":{},"outputs":[],"execution_count":null}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#,
    )?;
    let long_line = format!("{}beacon{}\n", "a".repeat(700), "b".repeat(700));
    fs::write(root.join("split.txt"), format!("head\n{long_line}tail\n"))?;
    fs::create_dir_all(root.join("vendor"))?;
    fs::write(root.join("vendor/hidden.txt"), "beacon in a hidden file\n")?;
    fs::write(root.join(".gitignore"), "vendor/\n")?;
    let service = service_over(root)?;
    let store = published_store(&directory, &service).await?;
    let request = params(json!({
        "pattern": "beacon",
        "target": "file",
        "paths": {"force_include": ["vendor/**"]},
    }))?;
    let answer = store_answer(&store, &service, &request, PatternBounds::default()).await?;
    let selected: Vec<&str> = answer
        .pattern_candidates()
        .ok_or("the store answered")?
        .candidates()
        .iter()
        .map(|candidate| candidate.path().as_str())
        .collect();
    assert!(
        !selected.contains(&"notes.ipynb"),
        "a cell row selects no file: {selected:?}"
    );
    let result = service.search(&request, &answer)?;
    let paths: Vec<String> = located(&result)
        .into_iter()
        .map(|(path, ..)| path)
        .collect();
    assert_eq!(paths, ["notes.ipynb", "split.txt", "vendor/hidden.txt"]);
    Ok(())
}

#[tokio::test]
async fn paths_narrow_the_verified_files() -> TestResult {
    let (directory, service) = fixture()?;
    let result = searched(
        &directory,
        &service,
        json!({"pattern": "TODO", "paths": {"include": ["src/**"]}}),
    )
    .await?;
    let paths: Vec<String> = located(&result)
        .into_iter()
        .map(|(path, ..)| path)
        .collect();
    assert_eq!(paths, ["src/lib.rs"]);
    Ok(())
}
