use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry};
use rift_protocol::documentation::{DocumentationContext, DocumentationHit};
use rift_protocol::error::{
    ErrorCause, ErrorCode, ErrorData, ErrorPhase, LimitEvidence, RetryDirective,
};
use rift_protocol::map::WorkspaceMap;
use rift_protocol::read::{
    CommitAuthor, DiagnosticContext, Digest, Documentation, DocumentationFormat, ExactKind,
    Extensions, FileId, GetSymbolHit, GetSymbolResult, Language, Node, NodeFacet, NodeId,
    NodeRegion, NodesResult, PackageIdentity, Pagination, ProjectPath, ReadWarning, RegionRole,
    RelationshipFacet, RevisionId, Signature, SourceKind, SourceLocationKind, SourceUnitId, Symbol,
    SymbolFacet, SymbolHistory, SymbolId, SymbolOrigin, SymbolVersion, SymbolVersionKind,
    TextRange,
};
use rift_protocol::search::{
    CommitHit, GraphHop, MatchedField, SearchHit, SearchHitTarget, SearchResult, SymbolChange,
};
use rift_protocol::workspace::WorkspaceResourcePage;
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use strum::VariantArray;

use super::facts::{cut_hash, date_of, moment_of, spaced_name, wire_name, wire_names};
use super::inline::fields_of;
use super::{RegisteredFailure, Render, text_of};
use crate::output::text::{INDENT_UNIT, OutputOverflow, TextError, TextWriter};

fn rendered<T: Render>(answer: &T) -> String {
    text_of(answer).expect("answer renders")
}

/// Asserts that no line of `output` ends in a space.
fn assert_no_trailing_space(output: &str) {
    for line in output.split('\n') {
        assert!(!line.ends_with(' '), "trailing space in {line:?}");
    }
}

/// Asserts that `answer` renders as exactly `expected`, one entry per line, and that no line of
/// the text ends in a space.
fn golden<T: Render>(answer: &T, expected: &[&str]) {
    let output = rendered(answer);
    assert_no_trailing_space(&output);
    assert_eq!(output, text(expected));
}

/// The authored schema example at `index` of `T`, deserialized into the typed value.
fn example<T: DeserializeOwned + JsonSchema>(index: usize) -> T {
    let schema = serde_json::to_value(schema_for!(T)).expect("schema serializes");
    let example = schema["examples"][index].clone();
    serde_json::from_value(example).expect("authored example deserializes")
}

/// Every authored schema example of `T`, deserialized into the typed value.
fn examples_of<T: DeserializeOwned + JsonSchema>() -> Vec<T> {
    let schema = serde_json::to_value(schema_for!(T)).expect("schema serializes");
    schema["examples"]
        .as_array()
        .expect("the type carries authored examples")
        .iter()
        .map(|example| serde_json::from_value(example.clone()).expect("example deserializes"))
        .collect()
}

/// Asserts that the compact text of each authored example of `T` is shorter in bytes than the
/// JSON text of the same answer.
fn assert_text_shorter_than_json<T: Render + Serialize + DeserializeOwned + JsonSchema>() {
    let answers = examples_of::<T>();
    assert!(
        !answers.is_empty(),
        "{} has no example",
        std::any::type_name::<T>()
    );
    for answer in &answers {
        let json = serde_json::to_value(answer)
            .expect("answer serializes")
            .to_string();
        let compact = rendered(answer);
        assert!(
            compact.len() < json.len(),
            "text {} bytes, JSON {} bytes: {compact}",
            compact.len(),
            json.len()
        );
    }
}

fn rust() -> Language {
    Language {
        name: "rust".to_owned(),
        dialect: None,
    }
}

fn pagination(page_index: u64, total_pages: u64) -> Pagination {
    Pagination {
        page_index,
        total_pages,
    }
}

fn symbol(id: Option<&str>, name: &str) -> Symbol {
    Symbol {
        id: id.map(|id| SymbolId(id.to_owned())),
        language: rust(),
        name: name.to_owned(),
        kind: ExactKind("struct".to_owned()),
        facets: vec![SymbolFacet::Type, SymbolFacet::Public],
        origin: SymbolOrigin {
            location: Some(SourceLocationKind::Project),
            package: None,
            source_kind: SourceKind::Authored,
        },
        container: None,
        modifiers: Vec::new(),
        visibility: None,
        types: Vec::new(),
        signatures: Vec::new(),
        documentation: Vec::new(),
        extensions: Extensions::default(),
        document_local: false,
    }
}

fn signature(display: &str) -> Signature {
    serde_json::from_value(json!({"display": display, "language": "rust"}))
        .expect("signature deserializes")
}

/// A symbol whose first signature displays `display`.
fn signed_symbol(id: Option<&str>, name: &str, display: &str) -> Symbol {
    Symbol {
        signatures: vec![signature(display)],
        ..symbol(id, name)
    }
}

fn documentation_text(text: &str) -> Documentation {
    Documentation {
        format: DocumentationFormat::Markdown,
        text: text.to_owned(),
    }
}

fn search_hit(target: SearchHitTarget) -> SearchHit {
    SearchHit {
        hit: target,
        score: None,
        matched_by: Vec::new(),
        source: None,
        range: None,
        line: None,
        path: None,
        unit: None,
        traversal_path: None,
        distance: None,
        change: None,
    }
}

fn symbol_target(id: Option<&str>, name: &str) -> SearchHitTarget {
    SearchHitTarget::Symbol {
        symbol: Box::new(symbol(id, name)),
    }
}

/// A symbol hit with the symbol `a.rs/A`, placed at `a.rs` line 1 and matched by name.
fn placed_symbol_hit(symbol: Symbol) -> SearchHit {
    SearchHit {
        matched_by: vec![MatchedField::Name],
        line: Some(1),
        path: Some(project_path("a.rs")),
        ..search_hit(SearchHitTarget::Symbol {
            symbol: Box::new(symbol),
        })
    }
}

fn range(start: u64, end: u64) -> TextRange {
    TextRange { start, end }
}

fn project_path(path: &str) -> ProjectPath {
    ProjectPath(path.to_owned())
}

fn source_unit(unit: &str) -> SourceUnitId {
    SourceUnitId(unit.to_owned())
}

fn search_result(
    results: Vec<SearchHit>,
    pagination: Pagination,
    warnings: Vec<ReadWarning>,
) -> SearchResult {
    SearchResult {
        results,
        pagination,
        warnings,
    }
}

/// An answer of one page that holds all of `results`.
fn only_page(results: Vec<SearchHit>) -> SearchResult {
    search_result(results, pagination(0, 1), Vec::new())
}

fn author() -> CommitAuthor {
    CommitAuthor {
        name: "Alice".to_owned(),
        email: "alice@example.com".to_owned(),
    }
}

fn commit_hit() -> CommitHit {
    CommitHit {
        revision: RevisionId("1f2080e49da12fee4431e6872630509355cd62d1".to_owned()),
        message: "Fix the cache\n\nBody line".to_owned(),
        message_truncated: false,
        author: author(),
        timestamp: "2026-08-21T14:03:22+00:00".to_owned(),
        paths: vec![project_path("src/a.rs"), project_path("src/b.rs")],
        paths_truncated: false,
    }
}

fn commit_target(commit: CommitHit) -> SearchHit {
    search_hit(SearchHitTarget::Commit {
        commit: Box::new(commit),
    })
}

fn stale_index() -> ReadWarning {
    ReadWarning::StaleIndex {
        index_tree_revision: Digest("3f9a1c2e".to_owned()),
        captured_tree_revision: Digest("3f9a1c2f".to_owned()),
        detail: "index lags the captured tree".to_owned(),
    }
}

fn node(id: &str, kind: &str, start: u64, end: u64) -> Node {
    Node {
        id: NodeId(id.to_owned()),
        symbol: None,
        unit: FileId("rift://file/src/lib.rs".to_owned()),
        language: rust(),
        kind: ExactKind(kind.to_owned()),
        facets: Vec::new(),
        range: range(start, end),
        regions: Vec::new(),
        parent: None,
        extensions: Extensions::default(),
    }
}

/// Joins `lines` with a line feed after each, the way the writer ends every line.
fn text(lines: &[&str]) -> String {
    let mut joined = lines.join("\n");
    joined.push('\n');
    joined
}

/// A documentation hit whose block sits at `place` (a project path) on `line`, under `headings`.
fn documentation_hit_at(
    place: &str,
    line: u64,
    headings: &[&str],
    symbol: Option<&str>,
) -> DocumentationHit {
    let heading_path: Vec<_> = headings
        .iter()
        .map(|name| json!({"level": 1, "name": name}))
        .collect();
    let mut block = json!({
        "identity": "a".repeat(64),
        "source": {"source": {"kind": "project", "path": place}},
        "content_digest": "b".repeat(64),
        "heading_path": heading_path,
        "range": {"start": 0, "end": 40},
        "line": line,
        "kind": "prose"
    });
    if let Some(symbol) = symbol {
        block["symbol"] = json!(symbol);
    }
    serde_json::from_value(json!({
        "block": block,
        "source": {
            "identity": {"source": {"kind": "project", "path": place}},
            "revision": "c".repeat(64),
            "content_digest": "d".repeat(64),
            "origin": {"location": "project", "source_kind": "authored"},
            "format": "markdown",
            "media_type": "text/markdown",
            "selection": "workspace",
            "byte_length": 40
        },
        "documentation_revision": "3f9a1c2e"
    }))
    .expect("documentation hit deserializes")
}

fn documentation_hit() -> DocumentationHit {
    documentation_hit_at("docs/guide.md", 1, &["Guide"], None)
}

/// A hop from `a.rs/A` to `b.rs/B` of the given direction, derivation, and confidence.
fn hop(direction: &str, derivation: &str, confidence: Option<f64>) -> GraphHop {
    let mut relationship = json!({
        "from": "rift://symbol/rust/a.rs/A",
        "kind": "calls",
        "facets": ["calls"],
        "to": "rift://symbol/rust/b.rs/B",
        "derivation": derivation
    });
    if let Some(confidence) = confidence {
        relationship["confidence"] = json!(confidence);
    }
    serde_json::from_value(json!({"relationship": relationship, "direction": direction}))
        .expect("graph hop deserializes")
}

fn graph_hop() -> GraphHop {
    hop("outgoing", "resolution", None)
}

fn symbol_hit_with_source(source: Option<&str>) -> GetSymbolHit {
    GetSymbolHit {
        symbol: symbol(Some("rift://symbol/rust/a.rs/A"), "A"),
        path: Some(project_path("a.rs")),
        unit: None,
        range: range(0, 9),
        line: 1,
        node: None,
        source: source.map(str::to_owned),
        history: None,
        documentation: None,
    }
}

fn symbol_result(hits: Vec<GetSymbolHit>, page: Pagination) -> GetSymbolResult {
    GetSymbolResult {
        hits,
        pagination: page,
        warnings: Vec::new(),
    }
}

fn nodes_result(nodes: Vec<Node>, source: Vec<&str>) -> NodesResult {
    NodesResult {
        nodes,
        source: source.into_iter().map(str::to_owned).collect(),
        warnings: Vec::new(),
    }
}

fn error_data(code: ErrorCode, message: &str) -> ErrorData {
    ErrorData {
        code,
        message: message.to_owned(),
        retry: RetryDirective::Never,
        phase: ErrorPhase::Read,
        diagnostics: Vec::new(),
        limit: None,
        causes: Vec::new(),
    }
}

// Owner examples.

const A_ID: &str = "rift://symbol/rust/a.rs/A";

/// The first authored search example with the source of its symbol hit cut to the declaration, as
/// the owner's example shows it. The authored example keeps the doc comment line in that source.
fn owner_search_page() -> SearchResult {
    let mut answer = example::<SearchResult>(0);
    answer.results[0].source = Some(
        "pub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}"
            .to_owned(),
    );
    answer
}

/// The second authored search example with the message body the owner's example shows, which
/// quotes `change_truncated` in backticks where the authored message does not.
fn owner_commit_answer() -> SearchResult {
    let mut answer = example::<SearchResult>(1);
    let SearchHitTarget::Commit { commit } = &mut answer.results[0].hit else {
        panic!("the second authored example is a commit hit");
    };
    commit.message = "Bound comparisons at 512 changed paths\n\nA comparison that reaches the bound answers from the paths that fit and warns `change_truncated`.\n".to_owned();
    answer
}

#[test]
fn the_owner_search_example_with_two_hits_on_the_first_of_three_pages() {
    golden(
        &owner_search_page(),
        &[
            "2 results · page 1/3",
            "\t[1] pub fn load_config(path: &Path) -> Result<Config, ConfigError>",
            "\t\tsrc/config.rs:10 · name",
            "\t\trift://symbol/rust/src/config.rs/load_config",
            "\t\tLoads the workspace configuration from `rift.toml`.",
            "",
            "\t\tpub fn load_config(path: &Path) -> Result<Config, ConfigError> {",
            "\t\t    let text = std::fs::read_to_string(path)?;",
            "\t\t    parse_config(&text)",
            "\t\t}",
            "\t[2] src/lib.rs:7 · content",
            "\t\t    let config = load_config(&arguments.path)?;",
        ],
    );
}

#[test]
fn the_owner_search_example_with_one_commit_hit() {
    golden(
        &owner_commit_answer(),
        &[
            "1 result",
            "\t9c1d4e7a · Alice <alice@example.com> · 2026-09-22 14:03 +02:00",
            "\tBound comparisons at 512 changed paths",
            "",
            "\tA comparison that reaches the bound answers from the paths that fit and warns `change_truncated`.",
            "",
            "\t2 paths:",
            "\t\tcrates/rift-protocol/src/search.rs",
            "\t\tcrates/rift-server/src/change.rs",
        ],
    );
}

#[test]
fn the_authored_search_examples_render_as_the_layout_shows() {
    golden(
        &example::<SearchResult>(0),
        &[
            "2 results · page 1/3",
            "\t[1] pub fn load_config(path: &Path) -> Result<Config, ConfigError>",
            "\t\tsrc/config.rs:10 · name",
            "\t\trift://symbol/rust/src/config.rs/load_config",
            "\t\tLoads the workspace configuration from `rift.toml`.",
            "",
            "\t\t/// Loads the workspace configuration from `rift.toml`.",
            "\t\tpub fn load_config(path: &Path) -> Result<Config, ConfigError> {",
            "\t\t    let text = std::fs::read_to_string(path)?;",
            "\t\t    parse_config(&text)",
            "\t\t}",
            "\t[2] src/lib.rs:7 · content",
            "\t\t    let config = load_config(&arguments.path)?;",
        ],
    );
    golden(
        &example::<SearchResult>(1),
        &[
            "1 result",
            "\t9c1d4e7a · Alice <alice@example.com> · 2026-09-22 14:03 +02:00",
            "\tBound comparisons at 512 changed paths",
            "",
            "\tA comparison that reaches the bound answers from the paths that fit and warns change_truncated.",
            "",
            "\t2 paths:",
            "\t\tcrates/rift-protocol/src/search.rs",
            "\t\tcrates/rift-server/src/change.rs",
        ],
    );
}

// Header and pages.

#[test]
fn zero_results_write_the_header_alone() {
    let answer = search_result(Vec::new(), pagination(0, 0), Vec::new());
    golden(&answer, &["0 results"]);
}

#[test]
fn one_result_has_no_marker_and_an_indent_of_two_spaces() {
    let hit = SearchHit {
        source: Some("struct A;".to_owned()),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    golden(
        &only_page(vec![hit]),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1 · name",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\tstruct A;",
        ],
    );
}

#[test]
fn several_results_on_one_page_have_markers_and_indent_and_no_page() {
    let first = SearchHit {
        source: Some("struct A {\n\n    x: u8,\n}".to_owned()),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    let second = placed_symbol_hit(symbol(Some("rift://symbol/rust/a.rs/B"), "B"));
    golden(
        &only_page(vec![first, second]),
        &[
            "2 results",
            "\t[1] struct A",
            "\t\ta.rs:1 · name",
            "\t\trift://symbol/rust/a.rs/A",
            "",
            "\t\tstruct A {",
            "",
            "\t\t    x: u8,",
            "\t\t}",
            "\t[2] struct B",
            "\t\ta.rs:1 · name",
            "\t\trift://symbol/rust/a.rs/B",
        ],
    );
}

#[test]
fn the_page_follows_the_page_index() {
    let item = || placed_symbol_hit(symbol(Some(A_ID), "A"));
    let body = [
        "\tstruct A",
        "\ta.rs:1 · name",
        "\trift://symbol/rust/a.rs/A",
    ];
    let page =
        |index: u64, total: u64| search_result(vec![item()], pagination(index, total), Vec::new());
    let expect = |header: &'static str| {
        let mut lines = vec![header];
        lines.extend(body);
        lines
    };
    golden(&page(0, 3), &expect("1 result · page 1/3"));
    golden(&page(1, 3), &expect("1 result · page 2/3"));
    golden(&page(2, 3), &expect("1 result · page 3/3"));
    golden(&page(0, 1), &expect("1 result"));
}

#[test]
fn a_page_past_the_end_names_the_page() {
    golden(
        &search_result(Vec::new(), pagination(5, 3), Vec::new()),
        &["0 results · page 6/3"],
    );
    golden(
        &search_result(Vec::new(), pagination(3, 3), Vec::new()),
        &["0 results · page 4/3"],
    );
    golden(
        &search_result(Vec::new(), pagination(4, 0), Vec::new()),
        &["0 results"],
    );
}

#[test]
fn the_largest_page_index_renders_without_overflow() {
    let answer = search_result(Vec::new(), pagination(u64::MAX, u64::MAX), Vec::new());
    assert_eq!(
        rendered(&answer),
        format!("0 results · page {}/{}\n", u64::MAX, u64::MAX)
    );
}

// Warnings.

fn warning_set() -> Vec<ReadWarning> {
    vec![
        ReadWarning::GlobalAccessDisabled,
        ReadWarning::ResultsTruncated { results_max: 100 },
        ReadWarning::PackageSubstituted {
            entry: PackageContextEntry {
                manager: "cargo".to_owned(),
                name: "serde".to_owned(),
                version: Some("1.0.0".to_owned()),
                requirement: None,
                availability: PackageAvailability::Canonical,
            },
            package: PackageIdentity {
                manager: "cargo".to_owned(),
                name: "serde".to_owned(),
                version: "1.0.1".to_owned(),
            },
        },
        ReadWarning::PatternMatchesTruncated {
            matches_per_file: 5,
            files: vec![
                FileId("rift://file/a.rs".to_owned()),
                FileId("rift://file/b.rs".to_owned()),
            ],
        },
        stale_index(),
    ]
}

const WARNING_LINES: [&str; 5] = [
    "global_access_disabled",
    "results_truncated · results_max 100",
    "package_substituted · entry serde@1.0.0 (cargo) · package serde@1.0.1 (cargo)",
    "pattern_matches_truncated · matches_per_file 5 · files rift://file/a.rs, rift://file/b.rs",
    "stale_index · index_tree_revision 3f9a1c2e · captured_tree_revision 3f9a1c2f: index lags the captured tree",
];

/// The warnings section of [`warning_set`].
const WARNING_SECTION: [&str; 6] = [
    "5 warnings",
    "\tglobal_access_disabled",
    "\tresults_truncated · results_max 100",
    "\tpackage_substituted · entry serde@1.0.0 (cargo) · package serde@1.0.1 (cargo)",
    "\tpattern_matches_truncated · matches_per_file 5 · files rift://file/a.rs, rift://file/b.rs",
    "\tstale_index · index_tree_revision 3f9a1c2e · captured_tree_revision 3f9a1c2f: index lags the captured tree",
];

#[test]
fn warnings_follow_one_item_in_their_own_section() {
    let hit = SearchHit {
        source: Some("struct A;".to_owned()),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    let answer = search_result(vec![hit], pagination(0, 2), warning_set());
    let mut expected = vec![
        "1 result · page 1/2",
        "\tstruct A",
        "\ta.rs:1 · name",
        "\trift://symbol/rust/a.rs/A",
        "",
        "\tstruct A;",
    ];
    expected.extend(WARNING_SECTION);
    golden(&answer, &expected);
}

#[test]
fn warnings_follow_several_items_without_a_blank_line() {
    let first = SearchHit {
        source: Some("struct A;".to_owned()),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    let second = placed_symbol_hit(symbol(Some("rift://symbol/rust/a.rs/B"), "B"));
    let answer = search_result(
        vec![first, second],
        pagination(0, 1),
        vec![
            ReadWarning::ResultsTruncated { results_max: 2 },
            stale_index(),
        ],
    );
    golden(
        &answer,
        &[
            "2 results",
            "\t[1] struct A",
            "\t\ta.rs:1 · name",
            "\t\trift://symbol/rust/a.rs/A",
            "",
            "\t\tstruct A;",
            "\t[2] struct B",
            "\t\ta.rs:1 · name",
            "\t\trift://symbol/rust/a.rs/B",
            "2 warnings",
            "\tresults_truncated · results_max 2",
            "\tstale_index · index_tree_revision 3f9a1c2e · captured_tree_revision 3f9a1c2f: index lags the captured tree",
        ],
    );
}

#[test]
fn warnings_are_written_when_the_answer_has_no_item() {
    let answer = search_result(Vec::new(), pagination(0, 0), warning_set());
    let mut expected = vec!["0 results"];
    expected.extend(WARNING_SECTION);
    golden(&answer, &expected);
}

#[test]
fn a_single_warning_without_items_is_a_section_of_one_warning() {
    let answer = search_result(
        Vec::new(),
        pagination(0, 0),
        vec![ReadWarning::GlobalAccessDisabled],
    );
    golden(
        &answer,
        &["0 results", "1 warning", "\tglobal_access_disabled"],
    );
}

#[test]
fn an_answer_without_warnings_writes_no_warnings_section() {
    let output = rendered(&only_page(vec![placed_symbol_hit(symbol(Some(A_ID), "A"))]));
    assert!(!output.contains("warning"), "{output}");
}

#[test]
fn warnings_follow_the_items_of_the_other_answers() {
    let mut symbols = symbol_result(Vec::new(), pagination(0, 0));
    symbols.warnings = vec![ReadWarning::GlobalAccessDisabled];
    golden(
        &symbols,
        &["0 results", "1 warning", "\tglobal_access_disabled"],
    );
    let mut nodes = nodes_result(Vec::new(), Vec::new());
    nodes.warnings = vec![stale_index(), ReadWarning::GlobalAccessDisabled];
    golden(
        &nodes,
        &[
            "0 nodes",
            "2 warnings",
            "\tstale_index · index_tree_revision 3f9a1c2e · captured_tree_revision 3f9a1c2f: index lags the captured tree",
            "\tglobal_access_disabled",
        ],
    );
}

/// The line of the warning that the wire form `wire` states.
fn warning_line(wire: &serde_json::Value) -> String {
    let warning: ReadWarning = serde_json::from_value(wire.clone()).expect("warning deserializes");
    super::warning::line(&warning).expect("warning renders")
}

/// Every warning variant with a wire form and the line it renders as.
#[expect(
    clippy::too_many_lines,
    reason = "one table pins the line of every warning variant"
)]
fn warning_cases() -> Vec<(serde_json::Value, &'static str)> {
    let file = |name: &str| format!("rift://file/{name}");
    let entry = |name: &str, selector: (&str, &str), availability: &str| json!({"manager": "cargo", "name": name, selector.0: selector.1, "availability": availability});
    vec![
        (
            json!({"code": "documentation", "warning": {
                "source": {"source": {"kind": "project", "path": "docs/a.md"}},
                "stage": "extract", "kind": "malformed_source", "count": 3}}),
            "documentation · warning (source docs/a.md, stage extract, kind malformed_source, count 3)",
        ),
        (
            json!({"code": "documentation_unavailable", "detail": "documentation crossed a bound"}),
            "documentation_unavailable: documentation crossed a bound",
        ),
        (
            json!({"code": "stale_index", "index_tree_revision": "3f9a1c2e",
                "captured_tree_revision": "3f9a1c2f", "detail": "index lags the captured tree"}),
            WARNING_LINES[4],
        ),
        (
            json!({"code": "vector_index_preparing", "prepared": 1200, "total": 4800,
                "ready_in": "45s", "detail": "Vector search is being prepared"}),
            "vector_index_preparing · prepared 1200 · total 4800 · ready_in 45s: Vector search is being prepared",
        ),
        (
            json!({"code": "local_index_preparing", "prepared": 10, "detail": "the local index is preparing"}),
            "local_index_preparing · prepared 10: the local index is preparing",
        ),
        (
            json!({"code": "local_index_preparing", "prepared": 10, "total": 40, "ready_in": "2m",
                "detail": "the local index is preparing"}),
            "local_index_preparing · prepared 10 · total 40 · ready_in 2m: the local index is preparing",
        ),
        (
            json!({"code": "vector_ranking_unavailable", "detail": "the model weights could not be acquired"}),
            "vector_ranking_unavailable: the model weights could not be acquired",
        ),
        (
            json!({"code": "lexical_ranking_unavailable", "detail": "the full-text index did not open"}),
            "lexical_ranking_unavailable: the full-text index did not open",
        ),
        (
            json!({"code": "query_narrowed", "terms_max": 32}),
            "query_narrowed · terms_max 32",
        ),
        (
            json!({"code": "lexical_ranking_truncated", "matches_max": 1000}),
            "lexical_ranking_truncated · matches_max 1000",
        ),
        (
            json!({"code": "results_truncated", "results_max": 100}),
            WARNING_LINES[1],
        ),
        (
            json!({"code": "pattern_matches_truncated", "matches_per_file": 5,
                "files": [file("a.rs"), file("b.rs")]}),
            WARNING_LINES[3],
        ),
        (
            json!({"code": "pattern_index_preparing", "prepared": 3, "total": 9, "detail": "rows are indexed in the background"}),
            "pattern_index_preparing · prepared 3 · total 9: rows are indexed in the background",
        ),
        (
            json!({"code": "source_unavailable", "unit": file("src%2Finvalid.rs"), "detail": "not UTF-8"}),
            "source_unavailable · unit rift://file/src%2Finvalid.rs: not UTF-8",
        ),
        (
            json!({"code": "source_unavailable", "detail": "2 more files are left out"}),
            "source_unavailable: 2 more files are left out",
        ),
        (
            json!({"code": "large_file_skipped", "skipped": 2, "detail": "past max_chunk"}),
            "large_file_skipped · skipped 2: past max_chunk",
        ),
        (
            json!({"code": "large_file_unparsed", "files": [file("big.rs")], "detail": "past max_file"}),
            "large_file_unparsed · files rift://file/big.rs: past max_file",
        ),
        (
            json!({"code": "lockfile_excluded", "files": [file("Cargo.lock")], "detail": "left out of search"}),
            "lockfile_excluded · files rift://file/Cargo.lock: left out of search",
        ),
        (
            json!({"code": "symbol_disagreement", "symbol": A_ID, "providers": ["history", "syntax"],
                "detail": "providers disagree"}),
            "symbol_disagreement · symbol rift://symbol/rust/a.rs/A · providers history, syntax: providers disagree",
        ),
        (
            json!({"code": "traversal_truncated", "visited": 500, "detail": "the walk stopped at its bound"}),
            "traversal_truncated · visited 500: the walk stopped at its bound",
        ),
        (
            json!({"code": "relationship_coverage_missing", "facets": ["calls", "imports"], "detail": "no lane"}),
            "relationship_coverage_missing · facets calls, imports: no lane",
        ),
        (
            json!({"code": "relationship_coverage_missing", "detail": "no lane"}),
            "relationship_coverage_missing: no lane",
        ),
        (
            json!({"code": "engine_analysis_unavailable", "language": "rust", "detail": "engine still analyzing"}),
            "engine_analysis_unavailable · language rust: engine still analyzing",
        ),
        (
            json!({"code": "callees_dropped", "callees": 4, "detail": "no declaration for them"}),
            "callees_dropped · callees 4: no declaration for them",
        ),
        (
            json!({"code": "engine_readiness_unconfirmed", "processes": ["rust-analyzer"], "detail": "no progress evidence"}),
            "engine_readiness_unconfirmed · processes rust-analyzer: no progress evidence",
        ),
        (
            json!({"code": "history_store_filling", "analyzed": 40, "total": 100, "detail": "40 of 100 commits"}),
            "history_store_filling · analyzed 40 · total 100: 40 of 100 commits",
        ),
        (
            json!({"code": "change_truncated", "paths_max": 512, "detail": "narrow the comparison"}),
            "change_truncated · paths_max 512: narrow the comparison",
        ),
        (
            json!({"code": "global_access_disabled"}),
            "global_access_disabled",
        ),
        (
            json!({"code": "global_api_unavailable", "failure_class": "retry_exhausted"}),
            "global_api_unavailable · failure_class retry_exhausted",
        ),
        (
            json!({"code": "global_publication_incompatible", "failure_class": "publication_format"}),
            "global_publication_incompatible · failure_class publication_format",
        ),
        (
            json!({"code": "global_response_invalid", "failure_class": "invalid_response"}),
            "global_response_invalid · failure_class invalid_response",
        ),
        (
            json!({"code": "global_page_warning", "warning_code": "capability_unavailable", "detail": "patterns"}),
            "global_page_warning · warning_code capability_unavailable: patterns",
        ),
        (
            json!({"code": "global_page_warning", "warning_code": "unknown"}),
            "global_page_warning · warning_code unknown",
        ),
        (
            json!({"code": "package_absent", "package": {"manager": "cargo", "name": "serde", "version": "1.0.0"}}),
            "package_absent · package serde@1.0.0 (cargo)",
        ),
        (
            json!({"code": "package_requirement_absent", "entry": entry("serde", ("requirement", "^1.0"), "canonical")}),
            "package_requirement_absent · entry serde ^1.0 (cargo)",
        ),
        (
            json!({"code": "package_substituted", "entry": entry("serde", ("version", "1.0.0"), "canonical"),
                "package": {"manager": "cargo", "name": "serde", "version": "1.0.1"}}),
            WARNING_LINES[2],
        ),
        (
            json!({"code": "package_unavailable", "entry": entry("local", ("version", "0.1.0"), "path"),
                "reason": "path dependencies are not indexed"}),
            "package_unavailable · entry local@0.1.0 (cargo, path) · reason path dependencies are not indexed",
        ),
        (
            json!({"code": "package_context_degraded", "resolver": "cargo", "reason": "metadata read failed"}),
            "package_context_degraded · resolver cargo · reason metadata read failed",
        ),
    ]
}

#[test]
fn every_warning_variant_renders_as_one_line() {
    let cases = warning_cases();
    for (wire, expected) in &cases {
        assert_eq!(&warning_line(wire), expected);
    }
    let schema = serde_json::to_value(schema_for!(ReadWarning)).expect("warning schema");
    let advertised: std::collections::BTreeSet<&str> = schema["oneOf"]
        .as_array()
        .expect("the warning union lists its arms")
        .iter()
        .filter_map(|arm| arm["properties"]["code"]["const"].as_str())
        .collect();
    let covered: std::collections::BTreeSet<&str> = cases
        .iter()
        .filter_map(|(wire, _)| wire["code"].as_str())
        .collect();
    assert_eq!(covered, advertised, "a case for every warning code");
}

#[test]
fn a_warning_value_or_detail_holding_a_delimiter_is_quoted_and_others_stay_bare() {
    assert_eq!(
        warning_line(
            &json!({"code": "package_context_degraded", "resolver": "cargo",
            "reason": "metadata: read \"failed\" · C:\\x"})
        ),
        r#"package_context_degraded · resolver cargo · reason "metadata: read \"failed\" · C:\\x""#
    );
    assert_eq!(
        warning_line(&json!({"code": "documentation_unavailable",
            "detail": "bound crossed: 2 files · 9 bytes"})),
        r#"documentation_unavailable: "bound crossed: 2 files · 9 bytes""#
    );
    assert_eq!(
        warning_line(
            &json!({"code": "package_context_degraded", "resolver": "cargo",
            "reason": "C:\\dir \"x\" a:b a·b"})
        ),
        r#"package_context_degraded · resolver cargo · reason C:\dir "x" a:b a·b"#
    );
}

#[test]
fn a_warning_line_shows_one_line_per_payload_shape() {
    let lines = [
        warning_line(&json!({"code": "global_access_disabled"})),
        warning_line(&json!({"code": "results_truncated", "results_max": 100})),
        warning_line(
            &json!({"code": "lockfile_excluded", "files": ["rift://file/Cargo.lock"],
            "detail": "left out of search"}),
        ),
        warning_line(&json!({"code": "package_absent",
            "package": {"manager": "cargo", "name": "serde", "version": "1.0.0"}})),
        warning_line(&json!({"code": "package_requirement_absent", "entry":
            {"manager": "npm", "name": "left-pad", "requirement": "^1.3", "availability": "canonical"}})),
    ];
    assert_eq!(
        lines,
        [
            "global_access_disabled",
            "results_truncated · results_max 100",
            "lockfile_excluded · files rift://file/Cargo.lock: left out of search",
            "package_absent · package serde@1.0.0 (cargo)",
            "package_requirement_absent · entry left-pad ^1.3 (npm)",
        ]
    );
}

#[test]
fn control_characters_in_a_detail_are_made_visible_and_the_warning_stays_one_line() {
    let wire = json!({"code": "traversal_truncated", "visited": 1,
        "detail": "first\nsecond\r\n\tthird\u{1b}[0m \n"});
    let line = warning_line(&wire);
    assert_eq!(
        line,
        "traversal_truncated · visited 1: first\\nsecond\\r\\n\\tthird\\u{1b}[0m"
    );
    let answer = search_result(
        Vec::new(),
        pagination(0, 0),
        vec![serde_json::from_value(wire).expect("warning")],
    );
    golden(&answer, &["0 results", "1 warning", &format!("\t{line}")]);
}

#[test]
fn a_detail_that_is_blank_or_ends_in_spaces_leaves_no_trailing_space() {
    let blank = json!({"code": "traversal_truncated", "visited": 1, "detail": "   "});
    assert_eq!(warning_line(&blank), "traversal_truncated · visited 1");
    let spaced = json!({"code": "vector_ranking_unavailable", "detail": "ends here  "});
    assert_eq!(
        warning_line(&spaced),
        "vector_ranking_unavailable: ends here"
    );
}

#[test]
fn a_long_hex_evidence_value_is_cut_to_eight_characters() {
    let wire = json!({"code": "package_context_degraded",
        "resolver": "0123456789abcdef0123456789abcdef", "reason": "x"});
    assert_eq!(
        warning_line(&wire),
        "package_context_degraded · resolver 01234567 · reason x"
    );
}

#[derive(Serialize)]
struct Pair {
    left: u8,
    right: Option<&'static str>,
}

#[derive(Serialize)]
struct Single {
    only: Pair,
}

#[derive(Serialize)]
enum Shape {
    Unit,
    Payload(u8),
    Tuple(u8, u8),
    Fields { x: u8 },
}

#[derive(Serialize)]
struct Scalars {
    flag: bool,
    small: i8,
    wide: i64,
    unsigned: u32,
    ratio: f32,
    exact: f64,
    letter: char,
    nothing: (),
    pair: Pair,
    single: Single,
    list: Vec<Option<u8>>,
    unit_variant: Shape,
    empty: Vec<u8>,
}

struct Bytes;

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&[1])
    }
}

#[test]
fn the_inline_writer_writes_each_scalar_and_nested_shape_on_one_line() {
    let value = Scalars {
        flag: true,
        small: -3,
        wide: 1 << 40,
        unsigned: 7,
        ratio: 0.5,
        exact: 2.25,
        letter: 'x',
        nothing: (),
        pair: Pair {
            left: 1,
            right: Some("r"),
        },
        single: Single {
            only: Pair {
                left: 2,
                right: None,
            },
        },
        list: vec![Some(1), None, Some(3)],
        unit_variant: Shape::Unit,
        empty: Vec::new(),
    };
    let fields = fields_of(&value).expect("fields");
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(key, text)| (*key, text.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("flag", "true"),
            ("small", "-3"),
            ("wide", "1099511627776"),
            ("unsigned", "7"),
            ("ratio", "0.5"),
            ("exact", "2.25"),
            ("letter", "x"),
            ("pair", "(left 1, right r)"),
            ("single", "2"),
            ("list", "1, 3"),
            ("unit_variant", "Unit"),
        ]
    );
}

#[test]
fn the_inline_writer_refuses_shapes_it_does_not_write() {
    let refused = |outcome: Result<Vec<(&str, String)>, TextError>| match outcome {
        Err(TextError::Unsupported(shape)) => shape,
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert_eq!(refused(fields_of(&"text")), "value that is not a struct");
    assert_eq!(refused(fields_of(&Bytes)), "bytes");
    assert_eq!(refused(fields_of(&(1, 2))), "tuple");
    assert_eq!(
        refused(fields_of(&std::collections::BTreeMap::from([(1, 2)]))),
        "map"
    );
    assert_eq!(
        refused(fields_of(&Shape::Payload(1))),
        "variant with a payload"
    );
    assert_eq!(refused(fields_of(&Shape::Tuple(1, 2))), "tuple variant");
    assert_eq!(
        refused(fields_of(&Shape::Fields { x: 1 })),
        "struct variant"
    );
    assert_eq!(refused(fields_of(&Marker(1, 2))), "tuple struct");
}

#[derive(Serialize)]
struct Marker(u8, u8);

#[derive(Serialize)]
struct Empty;

#[derive(Serialize)]
struct Wrapped(u8);

#[derive(Serialize)]
struct Wrappers {
    empty: Empty,
    wrapped: Wrapped,
}

#[test]
fn the_inline_writer_leaves_out_a_unit_struct_and_writes_a_newtype_struct_as_its_value() {
    let fields = fields_of(&Wrappers {
        empty: Empty,
        wrapped: Wrapped(9),
    })
    .expect("fields");
    assert_eq!(fields, [("wrapped", "9".to_owned())]);
}

// Symbol hits.

#[test]
fn a_symbol_hit_names_its_declaration_place_identity_and_summary() {
    let mut symbol = signed_symbol(
        Some("rift://symbol/rust/src/config.rs/load_config"),
        "load_config",
        "pub fn load_config(path: &Path)",
    );
    symbol.visibility = Some("pub".to_owned());
    symbol.documentation = vec![documentation_text(
        "\n  Loads the configuration.  \n\nSecond paragraph.",
    )];
    let hit = SearchHit {
        matched_by: vec![MatchedField::Name, MatchedField::Signature],
        line: Some(10),
        path: Some(project_path("src/config.rs")),
        ..search_hit(SearchHitTarget::Symbol {
            symbol: Box::new(symbol),
        })
    };
    golden(
        &only_page(vec![hit]),
        &[
            "1 result",
            "\tpub fn load_config(path: &Path)",
            "\tsrc/config.rs:10 · name, signature",
            "\trift://symbol/rust/src/config.rs/load_config",
            "\tLoads the configuration.",
        ],
    );
}

#[test]
fn the_visibility_is_written_once_before_a_declaration_that_lacks_it() {
    let with = |visibility: &str, display: &str| {
        let mut symbol = signed_symbol(Some(A_ID), "run", display);
        symbol.visibility = Some(visibility.to_owned());
        only_page(vec![placed_symbol_hit(symbol)])
    };
    let head = |answer: &SearchResult| {
        rendered(answer)
            .lines()
            .nth(1)
            .and_then(|line| line.strip_prefix("\t"))
            .map(str::to_owned)
    };
    assert_eq!(
        head(&with("pub", "fn run()")).as_deref(),
        Some("pub fn run()")
    );
    assert_eq!(
        head(&with("pub", "pub fn run()")).as_deref(),
        Some("pub fn run()")
    );
    assert_eq!(
        head(&with("pub(crate)", "pub(crate) fn run()")).as_deref(),
        Some("pub(crate) fn run()")
    );
    assert_eq!(
        head(&with("pub", "public_name()")).as_deref(),
        Some("pub public_name()")
    );
    assert_eq!(head(&with("pub", "pub")).as_deref(), Some("pub"));
    assert_eq!(head(&with("", "fn run()")).as_deref(), Some("fn run()"));
}

#[test]
fn a_symbol_without_a_signature_is_visibility_kind_and_name() {
    let mut visible = symbol(Some(A_ID), "A");
    visible.visibility = Some("pub".to_owned());
    let hidden = symbol(Some("rift://symbol/rust/a.rs/B"), "B");
    golden(
        &only_page(vec![placed_symbol_hit(visible), placed_symbol_hit(hidden)]),
        &[
            "2 results",
            "\t[1] pub struct A",
            "\t\ta.rs:1 · name",
            "\t\trift://symbol/rust/a.rs/A",
            "\t[2] struct B",
            "\t\ta.rs:1 · name",
            "\t\trift://symbol/rust/a.rs/B",
        ],
    );
}

#[test]
fn a_symbol_without_an_identity_shows_its_byte_range_when_the_hit_has_one() {
    let ranged = SearchHit {
        range: Some(range(162, 355)),
        ..placed_symbol_hit(symbol(None, "A"))
    };
    let unranged = placed_symbol_hit(symbol(None, "B"));
    golden(
        &only_page(vec![ranged, unranged]),
        &[
            "2 results",
            "\t[1] struct A",
            "\t\ta.rs:1 · name",
            "\t\t162..355",
            "\t[2] struct B",
            "\t\ta.rs:1 · name",
        ],
    );
}

#[test]
fn an_identity_replaces_the_byte_range() {
    let hit = SearchHit {
        range: Some(range(162, 355)),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    let output = rendered(&only_page(vec![hit]));
    assert!(!output.contains("162..355"), "{output}");
}

#[test]
fn a_hit_without_a_place_or_matched_field_writes_no_facts_line() {
    let hit = search_hit(symbol_target(Some(A_ID), "A"));
    golden(
        &only_page(vec![hit]),
        &["1 result", "\tstruct A", "\trift://symbol/rust/a.rs/A"],
    );
}

#[test]
fn a_dependency_hit_is_placed_by_its_unit() {
    let unit = "rift://source/cargo/serde/lib.rs";
    let with_line = SearchHit {
        unit: Some(source_unit(unit)),
        line: Some(3),
        matched_by: vec![MatchedField::Name],
        ..search_hit(symbol_target(Some(A_ID), "A"))
    };
    let without_line = SearchHit {
        unit: Some(source_unit(unit)),
        ..search_hit(symbol_target(Some("rift://symbol/rust/a.rs/B"), "B"))
    };
    golden(
        &only_page(vec![with_line, without_line]),
        &[
            "2 results",
            "\t[1] struct A",
            "\t\trift://source/cargo/serde/lib.rs:3 · name",
            "\t\trift://symbol/rust/a.rs/A",
            "\t[2] struct B",
            "\t\trift://source/cargo/serde/lib.rs",
            "\t\trift://symbol/rust/a.rs/B",
        ],
    );
}

#[test]
fn the_score_follows_the_matched_fields_and_the_distance_is_not_written() {
    let hit = SearchHit {
        score: Some(0.8125),
        distance: Some(2),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    let whole = SearchHit {
        score: Some(2.0),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    golden(
        &only_page(vec![hit, whole]),
        &[
            "2 results",
            "\t[1] struct A",
            "\t\ta.rs:1 · name · score 0.8125",
            "\t\trift://symbol/rust/a.rs/A",
            "\t[2] struct A",
            "\t\ta.rs:1 · name · score 2",
            "\t\trift://symbol/rust/a.rs/A",
        ],
    );
}

fn package(name: &str, version: &str) -> PackageIdentity {
    PackageIdentity {
        manager: "cargo".to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
    }
}

fn origin(
    location: Option<SourceLocationKind>,
    package: Option<PackageIdentity>,
    source_kind: SourceKind,
) -> SymbolOrigin {
    SymbolOrigin {
        location,
        package,
        source_kind,
    }
}

/// The third line of the only hit of `answer`, without its indent: the line that lists the
/// facts when there are any.
fn facts_line(symbol: Symbol) -> String {
    let output = rendered(&only_page(vec![SearchHit {
        matched_by: Vec::new(),
        line: None,
        path: None,
        ..placed_symbol_hit(symbol)
    }]));
    let line = output.lines().nth(2).unwrap_or_default();
    line.strip_prefix("\t").unwrap_or(line).to_owned()
}

#[test]
fn every_exceptional_fact_is_written_in_order() {
    let mut everything = symbol(Some(A_ID), "A");
    everything.origin = origin(
        Some(SourceLocationKind::Dependency),
        Some(package("serde", "1.0.0")),
        SourceKind::Generated,
    );
    everything.facets = vec![SymbolFacet::Test, SymbolFacet::Deprecated];
    everything.document_local = true;
    let hit = SearchHit {
        score: Some(1.5),
        distance: Some(1),
        unit: Some(source_unit("rift://source/cargo/serde/lib.rs")),
        line: Some(3),
        matched_by: vec![MatchedField::Relationship],
        ..search_hit(SearchHitTarget::Symbol {
            symbol: Box::new(everything),
        })
    };
    golden(
        &only_page(vec![hit]),
        &[
            "1 result",
            "\tstruct A",
            "\trift://source/cargo/serde/lib.rs:3 · relationship · score 1.5 · dependency serde@1.0.0 · generated · deprecated · test · document local",
            "\trift://symbol/rust/a.rs/A",
        ],
    );
}

#[test]
fn the_origin_is_written_only_when_it_is_not_the_project_default() {
    let facts = |origin: SymbolOrigin| {
        let mut symbol = symbol(Some(A_ID), "A");
        symbol.origin = origin;
        facts_line(symbol)
    };
    let project = Some(SourceLocationKind::Project);
    assert_eq!(
        facts(origin(project, None, SourceKind::Authored)),
        "rift://symbol/rust/a.rs/A"
    );
    assert_eq!(
        facts(origin(
            project,
            Some(package("app", "0.1.0")),
            SourceKind::Authored
        )),
        "project app@0.1.0"
    );
    assert_eq!(
        facts(origin(
            project,
            Some(package("serde · core", "1.0.0")),
            SourceKind::Authored
        )),
        "project \"serde · core@1.0.0\""
    );
    assert_eq!(
        facts(origin(
            project,
            Some(package("serde", "1.0: 0")),
            SourceKind::Authored
        )),
        "project \"serde@1.0: 0\""
    );
    assert_eq!(
        facts(origin(
            project,
            Some(package("serde", "1.0:0")),
            SourceKind::Authored
        )),
        "project serde@1.0:0"
    );
    assert_eq!(
        facts(origin(
            Some(SourceLocationKind::Stdlib),
            None,
            SourceKind::Authored
        )),
        "stdlib"
    );
    assert_eq!(
        facts(origin(
            Some(SourceLocationKind::External),
            None,
            SourceKind::Generated
        )),
        "external · generated"
    );
    assert_eq!(
        facts(origin(project, None, SourceKind::Generated)),
        "generated"
    );
    assert_eq!(
        facts(origin(None, None, SourceKind::Synthetic)),
        "synthetic"
    );
}

#[test]
fn deprecated_test_and_document_local_are_written_only_when_they_hold() {
    let mut symbol = symbol(Some(A_ID), "A");
    symbol.facets = vec![SymbolFacet::Type, SymbolFacet::Public];
    assert_eq!(facts_line(symbol.clone()), "rift://symbol/rust/a.rs/A");
    symbol.facets.push(SymbolFacet::Test);
    assert_eq!(facts_line(symbol.clone()), "test");
    symbol.facets.push(SymbolFacet::Deprecated);
    assert_eq!(facts_line(symbol.clone()), "deprecated · test");
    symbol.document_local = true;
    assert_eq!(facts_line(symbol), "deprecated · test · document local");
}

/// A hop over the edge `from` to `to` with the given facets, followed in `direction`.
fn edge(direction: &str, (from, to): (&str, &str), facets: &[&str], derivation: &str) -> GraphHop {
    serde_json::from_value(json!({
        "relationship": {
            "from": format!("rift://symbol/rust/{from}"),
            "kind": "edge",
            "facets": facets,
            "to": format!("rift://symbol/rust/{to}"),
            "derivation": derivation
        },
        "direction": direction
    }))
    .expect("graph hop deserializes")
}

/// A resolved hop against the edge `from` to `to`, which has the facet `facet`.
fn against(from: &str, to: &str, facet: &str) -> GraphHop {
    edge("incoming", (from, to), &[facet], "resolution")
}

/// A symbol hit at `file/name` declared as `display`, placed at `line` of its file and matched
/// by its relationship, that the hops `hops` reached.
fn walked_hit(at: &str, display: &str, line: u64, hops: Vec<GraphHop>) -> SearchHit {
    let (file, name) = at.rsplit_once('/').expect("a symbol is `<file>/<name>`");
    SearchHit {
        matched_by: vec![MatchedField::Relationship],
        line: Some(line),
        path: Some(project_path(file)),
        traversal_path: Some(hops),
        ..search_hit(SearchHitTarget::Symbol {
            symbol: Box::new(signed_symbol(
                Some(&format!("rift://symbol/rust/{at}")),
                name,
                display,
            )),
        })
    }
}

const LOAD_CONFIG: &str = "src/config.rs/load_config";
const RUN: &str = "src/app.rs/run";
const MAIN: &str = "src/main.rs/main";
const RELOAD_CONFIG: &str = "src/watch.rs/reload_config";

/// The hit of `run`, one hop from `load_config`.
fn run_hit() -> SearchHit {
    walked_hit(
        RUN,
        "fn run()",
        20,
        vec![against(RUN, LOAD_CONFIG, "references")],
    )
}

/// The hit of `main`, two hops from `load_config`, through `run`.
fn main_hit() -> SearchHit {
    walked_hit(
        MAIN,
        "async fn main()",
        9,
        vec![
            against(RUN, LOAD_CONFIG, "references"),
            against(MAIN, RUN, "references"),
        ],
    )
}

/// The hit of `reload_config`, one hop from `load_config`.
fn reload_hit() -> SearchHit {
    walked_hit(
        RELOAD_CONFIG,
        "fn reload_config()",
        41,
        vec![against(RELOAD_CONFIG, LOAD_CONFIG, "calls")],
    )
}

/// The hit of `load_config`, outside the walk.
fn load_config_hit() -> SearchHit {
    SearchHit {
        matched_by: vec![MatchedField::Name],
        line: Some(10),
        path: Some(project_path("src/config.rs")),
        ..search_hit(SearchHitTarget::Symbol {
            symbol: Box::new(signed_symbol(
                Some("rift://symbol/rust/src/config.rs/load_config"),
                "load_config",
                "pub fn load_config()",
            )),
        })
    }
}

/// A file hit outside the walk.
fn readme_hit() -> SearchHit {
    SearchHit {
        matched_by: vec![MatchedField::Content],
        line: Some(3),
        path: Some(project_path("README.md")),
        source: Some("Reads configuration".to_owned()),
        ..search_hit(SearchHitTarget::File {
            size: 20,
            languages: Vec::new(),
        })
    }
}

#[test]
fn a_walk_hit_is_one_node_under_a_root_that_writes_its_identity() {
    golden(
        &only_page(vec![run_hit()]),
        &[
            "1 result",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn paths_that_share_a_prefix_share_its_nodes_and_children_follow_answer_order() {
    golden(
        &only_page(vec![reload_hit(), main_hit(), run_hit()]),
        &[
            "3 results",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ called",
            "\t\t\tby fn reload_config()",
            "\t\t\tin src/watch.rs:41",
            "\t\t\tat rift://symbol/rust/src/watch.rs/reload_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t\t↳ referenced",
            "\t\t\t\tby async fn main()",
            "\t\t\t\tin src/main.rs:9",
            "\t\t\t\tat rift://symbol/rust/src/main.rs/main",
        ],
    );
}

#[test]
fn a_middle_symbol_without_a_hit_writes_its_identity_alone() {
    golden(
        &only_page(vec![main_hit()]),
        &[
            "1 result",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t\t↳ referenced",
            "\t\t\t\tby async fn main()",
            "\t\t\t\tin src/main.rs:9",
            "\t\t\t\tat rift://symbol/rust/src/main.rs/main",
        ],
    );
}

#[test]
fn walk_hits_from_two_starting_symbols_make_two_trees_with_a_blank_line_between() {
    let serve = walked_hit(
        "src/serve.rs/handle",
        "fn handle()",
        5,
        vec![edge(
            "outgoing",
            ("src/serve.rs/serve", "src/serve.rs/handle"),
            &["calls"],
            "resolution",
        )],
    );
    golden(
        &only_page(vec![run_hit(), serve, reload_hit()]),
        &[
            "3 results",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t↳ called",
            "\t\t\tby fn reload_config()",
            "\t\t\tin src/watch.rs:41",
            "\t\t\tat rift://symbol/rust/src/watch.rs/reload_config",
            "",
            "\tat rift://symbol/rust/src/serve.rs/serve",
            "",
            "\t\t↳ calls",
            "\t\t\tfn handle()",
            "\t\t\tin src/serve.rs:5",
            "\t\t\tat rift://symbol/rust/src/serve.rs/handle",
        ],
    );
}

#[test]
fn one_hit_outside_the_walk_has_no_marker_and_a_blank_line_separates_it_from_the_root() {
    golden(
        &only_page(vec![run_hit(), readme_hit()]),
        &[
            "2 results",
            "\tREADME.md:3 · content",
            "\tReads configuration",
            "",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn several_hits_outside_the_walk_have_markers_that_count_only_them() {
    let loose = placed_symbol_hit(symbol(Some(A_ID), "A"));
    golden(
        &only_page(vec![readme_hit(), run_hit(), loose, main_hit()]),
        &[
            "4 results",
            "\t[1] README.md:3 · content",
            "\t\tReads configuration",
            "\t[2] struct A",
            "\t\ta.rs:1 · name",
            "\t\trift://symbol/rust/a.rs/A",
            "",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t\t↳ referenced",
            "\t\t\t\tby async fn main()",
            "\t\t\t\tin src/main.rs:9",
            "\t\t\t\tat rift://symbol/rust/src/main.rs/main",
        ],
    );
}

#[test]
fn a_hit_outside_the_walk_that_is_the_starting_symbol_becomes_the_root_and_is_no_item() {
    golden(
        &only_page(vec![load_config_hit(), run_hit()]),
        &[
            "2 results",
            "\tpub fn load_config()",
            "\tin src/config.rs:10 · name",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
    golden(
        &only_page(vec![readme_hit(), run_hit(), load_config_hit()]),
        &[
            "3 results",
            "\tREADME.md:3 · content",
            "\tReads configuration",
            "",
            "\tpub fn load_config()",
            "\tin src/config.rs:10 · name",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn a_root_that_is_a_hit_carries_in_and_at_and_an_item_symbol_hit_carries_neither() {
    let item = placed_symbol_hit(symbol(Some(A_ID), "A"));
    golden(
        &only_page(vec![item, load_config_hit(), run_hit()]),
        &[
            "3 results",
            "\tstruct A",
            "\ta.rs:1 · name",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\tpub fn load_config()",
            "\tin src/config.rs:10 · name",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn a_root_that_is_a_hit_without_a_location_writes_its_facts_without_in() {
    let root = SearchHit {
        line: None,
        path: None,
        ..load_config_hit()
    };
    golden(
        &only_page(vec![root, run_hit()]),
        &[
            "2 results",
            "\tpub fn load_config()",
            "\tname",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn a_root_that_is_no_hit_is_one_line_at_its_identity() {
    golden(
        &only_page(vec![run_hit()]),
        &[
            "1 result",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn a_walk_node_whose_hit_is_not_a_symbol_writes_nothing() {
    let mut out = TextWriter::new(1 << 10);
    let mut lines = super::layout::Lines::node(&mut out, 0, None);
    super::search::walk_node(&mut lines, &readme_hit(), super::walk::Labels::Root)
        .expect("a hit that is not a symbol writes nothing");
    assert_eq!(out.finish(), Ok(String::new()));
}

#[test]
fn two_walk_hits_that_reach_one_symbol_under_one_parent_each_get_a_node() {
    let again = SearchHit {
        line: Some(21),
        ..run_hit()
    };
    golden(
        &only_page(vec![run_hit(), again]),
        &[
            "2 results",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:21",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn an_outgoing_hop_writes_each_facet_as_its_wire_name_with_spaces() {
    let hop = edge(
        "outgoing",
        (MAIN, RUN),
        &["calls", "has_type", "depends_on"],
        "resolution",
    );
    golden(
        &only_page(vec![walked_hit(RUN, "fn run()", 20, vec![hop])]),
        &[
            "1 result",
            "\tat rift://symbol/rust/src/main.rs/main",
            "",
            "\t\t↳ calls, has type, depends on",
            "\t\t\tfn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn an_incoming_hop_writes_each_facet_read_from_its_target_back_and_the_distance_is_not_written() {
    let hop = edge(
        "incoming",
        (RUN, LOAD_CONFIG),
        &["calls", "references", "has_type"],
        "resolution",
    );
    let hit = SearchHit {
        distance: Some(1),
        ..walked_hit(RUN, "fn run()", 20, vec![hop])
    };
    golden(
        &only_page(vec![hit]),
        &[
            "1 result",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ called by, referenced by, type of",
            "\t\t\tfn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn a_heuristic_hop_starts_with_probably_whatever_its_confidence_and_direction() {
    let outgoing = edge("outgoing", (MAIN, RUN), &["implements"], "heuristic");
    let incoming = edge("incoming", (RUN, LOAD_CONFIG), &["implements"], "heuristic");
    let mut surer = incoming.clone();
    surer.relationship.confidence = Some(0.9);
    golden(
        &only_page(vec![
            walked_hit(RUN, "fn run()", 20, vec![outgoing]),
            walked_hit(RUN, "fn run()", 20, vec![incoming]),
            walked_hit(RUN, "fn run()", 20, vec![surer]),
        ]),
        &[
            "3 results",
            "\tat rift://symbol/rust/src/main.rs/main",
            "",
            "\t\t↳ probably implements",
            "\t\t\tfn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ probably implemented",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t↳ probably implemented",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

/// Each facet with the words an incoming hop writes for it.
const INCOMING_WORDS: [(&str, &str); 29] = [
    ("contains", "contained by"),
    ("declares", "declared by"),
    ("augments", "augmented by"),
    ("references", "referenced by"),
    ("calls", "called by"),
    ("constructs", "constructed by"),
    ("reads", "read by"),
    ("writes", "written by"),
    ("imports", "imported by"),
    ("exports", "exported by"),
    ("extends", "extended by"),
    ("implements", "implemented by"),
    ("has_type", "type of"),
    ("overrides", "overridden by"),
    ("aliases", "aliased by"),
    ("generates", "generated by"),
    ("depends_on", "dependency of"),
    ("annotated_by", "annotates"),
    ("throws", "thrown by"),
    ("catches", "caught by"),
    ("bounded_by", "bounds"),
    ("instantiates", "instantiated by"),
    ("specializes", "specialized by"),
    ("overloads", "overloaded by"),
    ("mixes_in", "mixed in by"),
    ("embeds", "embedded by"),
    ("tests", "tested by"),
    ("configures", "configured by"),
    ("binds", "bound by"),
];

#[test]
fn the_by_word_moves_to_the_declaration_line_only_when_every_facet_phrase_ends_in_it() {
    let hit = |facets: &[&str], derivation: &str| {
        walked_hit(
            RUN,
            "fn run()",
            20,
            vec![edge("incoming", (RUN, LOAD_CONFIG), facets, derivation)],
        )
    };
    let root = "\tat rift://symbol/rust/src/config.rs/load_config";
    let tail = [
        "\t\t\tin src/app.rs:20",
        "\t\t\tat rift://symbol/rust/src/app.rs/run",
    ];
    let written = |head: &str, declaration: &str, hit: SearchHit| {
        let mut expected = vec!["1 result", root, "", head, declaration];
        expected.extend(tail);
        golden(&only_page(vec![hit]), &expected);
    };
    written(
        "\t\t↳ called, referenced",
        "\t\t\tby fn run()",
        hit(&["calls", "references"], "resolution"),
    );
    written(
        "\t\t↳ called by, type of",
        "\t\t\tfn run()",
        hit(&["calls", "has_type"], "resolution"),
    );
    written(
        "\t\t↳ probably called",
        "\t\t\tby fn run()",
        hit(&["calls"], "heuristic"),
    );
    written(
        "\t\t↳ annotates",
        "\t\t\tfn run()",
        hit(&["annotated_by"], "resolution"),
    );
}

#[test]
fn an_outgoing_hop_moves_by_for_annotated_by_and_writes_calls_whole() {
    let walked = |facet: &str| {
        walked_hit(
            RUN,
            "fn run()",
            20,
            vec![edge("outgoing", (MAIN, RUN), &[facet], "resolution")],
        )
    };
    let tail = [
        "\t\t\tin src/app.rs:20",
        "\t\t\tat rift://symbol/rust/src/app.rs/run",
    ];
    let mut calls = vec![
        "1 result",
        "\tat rift://symbol/rust/src/main.rs/main",
        "",
        "\t\t↳ calls",
        "\t\t\tfn run()",
    ];
    calls.extend(tail);
    golden(&only_page(vec![walked("calls")]), &calls);
    let mut annotated = vec![
        "1 result",
        "\tat rift://symbol/rust/src/main.rs/main",
        "",
        "\t\t↳ annotated",
        "\t\t\tby fn run()",
    ];
    annotated.extend(tail);
    golden(&only_page(vec![walked("annotated_by")]), &annotated);
}

#[test]
fn a_node_without_a_location_writes_its_facts_without_in() {
    let hit = SearchHit {
        line: None,
        path: None,
        matched_by: vec![MatchedField::Relationship, MatchedField::Name],
        ..run_hit()
    };
    golden(
        &only_page(vec![hit]),
        &[
            "1 result",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tname",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn copied_source_keeps_its_own_leading_spaces_and_empty_lines_after_the_indent() {
    let source = "  two spaces\n\n\tone tab\n    four spaces";
    let node = SearchHit {
        source: Some(source.to_owned()),
        ..run_hit()
    };
    golden(
        &only_page(vec![node]),
        &[
            "1 result",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t\t  two spaces",
            "",
            "\t\t\t\tone tab",
            "\t\t\t    four spaces",
        ],
    );
    let item = SearchHit {
        source: Some(source.to_owned()),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    golden(
        &only_page(vec![item]),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1 · name",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\t  two spaces",
            "",
            "\t\tone tab",
            "\t    four spaces",
        ],
    );
}

#[test]
fn the_indent_unit_is_one_tab() {
    assert_eq!(INDENT_UNIT, "\t");
}

#[test]
fn no_line_of_a_sample_of_each_tool_starts_with_a_space() {
    let mut texts = vec![
        rendered(&example::<SearchResult>(0)),
        rendered(&example::<GetSymbolResult>(0)),
        rendered(&example::<NodesResult>(0)),
        rendered(&error_data(ErrorCode::InternalError, "first")),
        rendered(&search_result(
            vec![readme_hit(), main_hit(), reload_hit()],
            pagination(0, 1),
            vec![stale_index()],
        )),
    ];
    texts.extend(examples_of::<SearchResult>().iter().map(rendered));
    for text in texts {
        for line in text.lines() {
            assert!(
                !line.starts_with(' '),
                "a line starts with a space: {line:?} in {text}"
            );
        }
    }
}

#[test]
fn an_incoming_hop_reads_every_facet_as_the_table_says() {
    assert_eq!(INCOMING_WORDS.len(), RelationshipFacet::VARIANTS.len());
    for facet in RelationshipFacet::VARIANTS {
        let wire = facet.as_ref();
        let Some((_, words)) = INCOMING_WORDS.iter().find(|(name, _)| *name == wire) else {
            panic!("the table has no row for {wire}");
        };
        let hit = walked_hit(RUN, "fn run()", 20, vec![against(RUN, LOAD_CONFIG, wire)]);
        // A phrase that ends in ` by` moves it to the declaration line.
        let (head, declaration) = match words.strip_suffix(" by") {
            Some(stem) => (format!("\t\t↳ {stem}"), "\t\t\tby fn run()"),
            None => (format!("\t\t↳ {words}"), "\t\t\tfn run()"),
        };
        golden(
            &only_page(vec![hit]),
            &[
                "1 result",
                "\tat rift://symbol/rust/src/config.rs/load_config",
                "",
                &head,
                declaration,
                "\t\t\tin src/app.rs:20",
                "\t\t\tat rift://symbol/rust/src/app.rs/run",
            ],
        );
    }
}

#[test]
fn a_node_writes_every_line_of_a_symbol_hit_at_its_further_line_indent() {
    let mut everything = signed_symbol(
        Some("rift://symbol/rust/src/lib.rs/Config"),
        "Config",
        "struct Config",
    );
    everything.origin = origin(
        Some(SourceLocationKind::Dependency),
        Some(package("serde", "1.0.0")),
        SourceKind::Generated,
    );
    everything.facets = vec![SymbolFacet::Test, SymbolFacet::Deprecated];
    everything.document_local = true;
    everything.documentation = vec![documentation_text("Settings of a run.\n\nMore.")];
    let hit = SearchHit {
        score: Some(1.5),
        matched_by: vec![MatchedField::Relationship, MatchedField::Name],
        unit: Some(source_unit("rift://source/cargo/serde/lib.rs")),
        line: Some(3),
        source: Some("struct Config {\n\n    a: u8,\n}".to_owned()),
        change: Some(SymbolChange {
            kind: SymbolVersionKind::Moved,
            base_path: Some(project_path("old/lib.rs")),
            head_path: Some(project_path("new/lib.rs")),
        }),
        traversal_path: Some(vec![edge(
            "outgoing",
            (RUN, "src/lib.rs/Config"),
            &["constructs"],
            "resolution",
        )]),
        ..search_hit(SearchHitTarget::Symbol {
            symbol: Box::new(everything),
        })
    };
    golden(
        &only_page(vec![hit]),
        &[
            "1 result",
            "\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t↳ constructs",
            "\t\t\tstruct Config",
            "\t\t\tin rift://source/cargo/serde/lib.rs:3 · name · score 1.5 · dependency serde@1.0.0 · generated · deprecated · test · document local",
            "\t\t\tat rift://symbol/rust/src/lib.rs/Config",
            "\t\t\tSettings of a run.",
            "\t\t\tmoved · old/lib.rs → new/lib.rs",
            "",
            "\t\t\tstruct Config {",
            "",
            "\t\t\t    a: u8,",
            "\t\t\t}",
        ],
    );
}

#[test]
fn the_relationship_field_is_left_out_of_a_node_and_every_other_field_stays() {
    let only = SearchHit {
        score: Some(2.0),
        ..run_hit()
    };
    let named = SearchHit {
        matched_by: vec![MatchedField::Relationship, MatchedField::Name],
        ..reload_hit()
    };
    let bare = SearchHit {
        line: None,
        path: None,
        ..main_hit()
    };
    golden(
        &only_page(vec![only, named, bare]),
        &[
            "3 results",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20 · score 2",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t\t↳ referenced",
            "\t\t\t\tby async fn main()",
            "\t\t\t\tat rift://symbol/rust/src/main.rs/main",
            "",
            "\t\t↳ called",
            "\t\t\tby fn reload_config()",
            "\t\t\tin src/watch.rs:41 · name",
            "\t\t\tat rift://symbol/rust/src/watch.rs/reload_config",
        ],
    );
}

#[test]
fn an_item_keeps_the_relationship_field_among_its_matched_fields() {
    let item = SearchHit {
        traversal_path: None,
        ..run_hit()
    };
    golden(
        &only_page(vec![item]),
        &[
            "1 result",
            "\tfn run()",
            "\tsrc/app.rs:20 · relationship",
            "\trift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn a_hit_of_another_target_with_a_traversal_path_stays_an_item() {
    let file = SearchHit {
        traversal_path: Some(vec![graph_hop()]),
        ..readme_hit()
    };
    golden(
        &only_page(vec![file, run_hit()]),
        &[
            "2 results",
            "\tREADME.md:3 · content",
            "\tReads configuration",
            "",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tby fn run()",
            "\t\t\tin src/app.rs:20",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
        ],
    );
}

#[test]
fn a_traversal_without_hops_is_an_item() {
    let hit = SearchHit {
        traversal_path: Some(Vec::new()),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    golden(
        &only_page(vec![hit]),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1 · name",
            "\trift://symbol/rust/a.rs/A",
        ],
    );
}

#[test]
fn warnings_follow_the_last_node_without_a_blank_line() {
    let answer = search_result(
        vec![readme_hit(), main_hit()],
        pagination(0, 1),
        vec![stale_index()],
    );
    golden(
        &answer,
        &[
            "2 results",
            "\tREADME.md:3 · content",
            "\tReads configuration",
            "",
            "\tat rift://symbol/rust/src/config.rs/load_config",
            "",
            "\t\t↳ referenced",
            "\t\t\tat rift://symbol/rust/src/app.rs/run",
            "",
            "\t\t\t↳ referenced",
            "\t\t\t\tby async fn main()",
            "\t\t\t\tin src/main.rs:9",
            "\t\t\t\tat rift://symbol/rust/src/main.rs/main",
            "1 warning",
            "\tstale_index · index_tree_revision 3f9a1c2e · captured_tree_revision 3f9a1c2f: index lags the captured tree",
        ],
    );
}

fn changed(kind: SymbolVersionKind, base: Option<&str>, head: Option<&str>) -> SearchHit {
    SearchHit {
        change: Some(SymbolChange {
            kind,
            base_path: base.map(project_path),
            head_path: head.map(project_path),
        }),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    }
}

#[test]
fn a_change_writes_its_kind_and_the_path_when_the_declaration_moved() {
    let hits = vec![
        changed(SymbolVersionKind::Moved, Some("old/a.rs"), Some("new/a.rs")),
        changed(
            SymbolVersionKind::SignatureChanged,
            Some("a.rs"),
            Some("a.rs"),
        ),
        changed(SymbolVersionKind::Introduced, None, Some("a.rs")),
        changed(SymbolVersionKind::Removed, Some("gone/a.rs"), None),
        changed(SymbolVersionKind::BodyChanged, None, None),
        changed(SymbolVersionKind::Introduced, None, Some("new/a.rs")),
    ];
    let output = rendered(&only_page(hits));
    assert_no_trailing_space(&output);
    let changes: Vec<&str> = output
        .lines()
        .filter(|line| line.starts_with("\t\t") && !line.contains(':'))
        .collect();
    assert_eq!(
        changes,
        [
            "\t\tmoved · old/a.rs → new/a.rs",
            "\t\tsignature changed",
            "\t\tintroduced",
            "\t\tremoved · gone/a.rs",
            "\t\tbody changed",
            "\t\tintroduced · new/a.rs",
        ]
    );
}

#[test]
fn source_follows_a_blank_line_after_the_last_fact_of_a_symbol_hit() {
    let hit = SearchHit {
        source: Some("struct A;".to_owned()),
        ..changed(SymbolVersionKind::Moved, Some("old/a.rs"), Some("a.rs"))
    };
    golden(
        &only_page(vec![hit]),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1 · name",
            "\trift://symbol/rust/a.rs/A",
            "\tmoved · old/a.rs → a.rs",
            "",
            "\tstruct A;",
        ],
    );
}

#[test]
fn an_empty_source_of_a_symbol_hit_is_one_empty_line_after_the_blank_line() {
    let present = SearchHit {
        source: Some(String::new()),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    let missing = placed_symbol_hit(symbol(Some(A_ID), "A"));
    let with_empty = rendered(&only_page(vec![present]));
    let without = rendered(&only_page(vec![missing]));
    assert_eq!(with_empty, format!("{without}\n\n"));
    assert!(with_empty.ends_with("rift://symbol/rust/a.rs/A\n\n\n"));
}

// File, node, and documentation hits.

#[test]
fn a_file_hit_writes_its_place_and_the_source_directly_under_it() {
    let by_path = SearchHit {
        matched_by: vec![MatchedField::Content],
        line: Some(7),
        path: Some(project_path("src/lib.rs")),
        source: Some("line one\nline two".to_owned()),
        score: Some(3.0),
        ..search_hit(SearchHitTarget::File {
            size: 241,
            languages: vec![rust()],
        })
    };
    let by_unit = SearchHit {
        matched_by: vec![MatchedField::Path],
        unit: Some(source_unit("rift://source/cargo/serde/lib.rs")),
        ..search_hit(SearchHitTarget::File {
            size: 0,
            languages: Vec::new(),
        })
    };
    let bare = search_hit(SearchHitTarget::File {
        size: 3,
        languages: Vec::new(),
    });
    golden(
        &only_page(vec![by_path, by_unit, bare]),
        &[
            "3 results",
            "\t[1] src/lib.rs:7 · content · score 3",
            "\t\tline one",
            "\t\tline two",
            "\t[2] rift://source/cargo/serde/lib.rs · path",
            "\t[3]",
        ],
    );
}

#[test]
fn a_node_hit_writes_its_place_its_identity_and_the_source() {
    let node_id = "rift://node/rust/a.rs@0-9#3f9a1c2e";
    let placed = SearchHit {
        matched_by: vec![MatchedField::Content],
        line: Some(1),
        path: Some(project_path("a.rs")),
        source: Some("fn a() {}".to_owned()),
        ..search_hit(SearchHitTarget::Node {
            node: NodeId(node_id.to_owned()),
        })
    };
    let bare = search_hit(SearchHitTarget::Node {
        node: NodeId(node_id.to_owned()),
    });
    golden(
        &only_page(vec![placed, bare]),
        &[
            "2 results",
            "\t[1] a.rs:1 · content",
            "\t\trift://node/rust/a.rs@0-9#3f9a1c2e",
            "\t\tfn a() {}",
            "\t[2]",
            "\t\trift://node/rust/a.rs@0-9#3f9a1c2e",
        ],
    );
}

#[test]
fn a_documentation_hit_is_headed_by_its_heading_path() {
    let documented = SearchHit {
        matched_by: vec![MatchedField::Documentation],
        score: Some(1.5),
        source: Some("body\n\nmore".to_owned()),
        ..search_hit(SearchHitTarget::Documentation {
            documentation: Box::new(documentation_hit_at(
                "docs/guide.md",
                12,
                &["Guide", "Install", "Linux"],
                Some(A_ID),
            )),
        })
    };
    let plain = SearchHit {
        matched_by: vec![MatchedField::Documentation],
        ..search_hit(SearchHitTarget::Documentation {
            documentation: Box::new(documentation_hit_at("README.md", 1, &[], None)),
        })
    };
    golden(
        &only_page(vec![documented, plain]),
        &[
            "2 results",
            "\t[1] Guide > Install > Linux",
            "\t\tdocs/guide.md:12 · documentation · score 1.5",
            "\t\trift://symbol/rust/a.rs/A",
            "\t\tbody",
            "",
            "\t\tmore",
            "\t[2] documentation",
            "\t\tREADME.md:1 · documentation",
        ],
    );
}

#[test]
fn a_documentation_hit_in_a_package_is_placed_by_its_source_unit() {
    let mut hit = documentation_hit();
    hit.block.source.source = rift_protocol::documentation::DocumentationSourceIdentity::Package {
        unit: source_unit("rift://source/cargo/serde/README.md"),
    };
    let answer = only_page(vec![SearchHit {
        matched_by: vec![MatchedField::Documentation],
        ..search_hit(SearchHitTarget::Documentation {
            documentation: Box::new(hit),
        })
    }]);
    golden(
        &answer,
        &[
            "1 result",
            "\tGuide",
            "\trift://source/cargo/serde/README.md:1 · documentation",
        ],
    );
}

// Commit hits.

fn commit_answer(commit: CommitHit) -> SearchResult {
    only_page(vec![commit_target(commit)])
}

#[test]
fn a_commit_with_a_subject_alone_and_no_paths_is_one_line_and_the_subject() {
    let commit = CommitHit {
        message: "Fix the cache".to_owned(),
        paths: Vec::new(),
        ..commit_hit()
    };
    golden(
        &commit_answer(commit),
        &[
            "1 result",
            "\t1f2080e4 · Alice <alice@example.com> · 2026-08-21 14:03 +00:00",
            "\tFix the cache",
        ],
    );
}

#[test]
fn a_commit_with_a_body_writes_it_after_a_blank_line_and_the_paths_after_another() {
    golden(
        &commit_answer(commit_hit()),
        &[
            "1 result",
            "\t1f2080e4 · Alice <alice@example.com> · 2026-08-21 14:03 +00:00",
            "\tFix the cache",
            "",
            "\tBody line",
            "",
            "\t2 paths:",
            "\t\tsrc/a.rs",
            "\t\tsrc/b.rs",
        ],
    );
}

#[test]
fn a_commit_among_several_hits_has_marker_indent_and_empty_blank_lines() {
    let first = CommitHit {
        message: "Fix the cache\n\nBody line\n\n\nSecond paragraph\n\n".to_owned(),
        ..commit_hit()
    };
    let second = CommitHit {
        revision: RevisionId("2e3f4a5b6c7d8e9f".to_owned()),
        message: "Other".to_owned(),
        paths: Vec::new(),
        ..commit_hit()
    };
    golden(
        &only_page(vec![commit_target(first), commit_target(second)]),
        &[
            "2 results",
            "\t[1] 1f2080e4 · Alice <alice@example.com> · 2026-08-21 14:03 +00:00",
            "\t\tFix the cache",
            "",
            "\t\tBody line",
            "",
            "",
            "\t\tSecond paragraph",
            "",
            "\t\t2 paths:",
            "\t\t\tsrc/a.rs",
            "\t\t\tsrc/b.rs",
            "\t[2] 2e3f4a5b · Alice <alice@example.com> · 2026-08-21 14:03 +00:00",
            "\t\tOther",
        ],
    );
}

#[test]
fn a_truncated_message_ends_with_the_truncation_line_directly_after_it() {
    let with_body = CommitHit {
        message_truncated: true,
        paths: Vec::new(),
        ..commit_hit()
    };
    let subject_only = CommitHit {
        message: "Fix the cache".to_owned(),
        message_truncated: true,
        paths: Vec::new(),
        ..commit_hit()
    };
    golden(
        &only_page(vec![commit_target(with_body), commit_target(subject_only)]),
        &[
            "2 results",
            "\t[1] 1f2080e4 · Alice <alice@example.com> · 2026-08-21 14:03 +00:00",
            "\t\tFix the cache",
            "",
            "\t\tBody line",
            "\t\tmessage truncated",
            "\t[2] 1f2080e4 · Alice <alice@example.com> · 2026-08-21 14:03 +00:00",
            "\t\tFix the cache",
            "\t\tmessage truncated",
        ],
    );
}

#[test]
fn truncated_paths_are_labelled_and_missing_paths_write_no_label() {
    let truncated = CommitHit {
        message: "Fix".to_owned(),
        paths_truncated: true,
        ..commit_hit()
    };
    golden(
        &commit_answer(truncated),
        &[
            "1 result",
            "\t1f2080e4 · Alice <alice@example.com> · 2026-08-21 14:03 +00:00",
            "\tFix",
            "",
            "\t2+ paths · truncated",
            "\t\tsrc/a.rs",
            "\t\tsrc/b.rs",
            "\t\t...",
        ],
    );
    let none = CommitHit {
        message: "Fix".to_owned(),
        paths: Vec::new(),
        paths_truncated: true,
        ..commit_hit()
    };
    let output = rendered(&commit_answer(none));
    assert!(!output.contains("paths"), "{output}");
}

#[test]
fn the_timestamp_is_cut_to_date_hour_minute_and_offset_when_it_has_the_rfc_3339_shape() {
    assert_eq!(
        moment_of("2026-09-22T14:03:11+02:00"),
        "2026-09-22 14:03 +02:00"
    );
    assert_eq!(moment_of("2026-09-22T14:03:11Z"), "2026-09-22 14:03 Z");
    assert_eq!(
        moment_of("2026-09-22T14:03:11.250-05:30"),
        "2026-09-22 14:03 -05:30"
    );
    assert_eq!(date_of("2026-09-22T14:03:11Z"), "2026-09-22");
    assert_eq!(date_of("2026-09-22T14:03:11.5+01:00"), "2026-09-22");
    for malformed in [
        "",
        "yesterday",
        "2026-09-22",
        "2026-09-22 14:03:11Z",
        "2026-09-22T14:03:11",
        "2026-09-22T14:03:11+0200",
        "2026-09-22T14:03:11.Z",
        "2026-09-22T14:03:11+02:0x",
        "2026-09-22T14:03:11ZZ",
        "2026-09-2xT14:03:11Z",
        "2026-09-22T14:03:1é+02:00",
    ] {
        assert_eq!(moment_of(malformed), malformed);
        assert_eq!(date_of(malformed), malformed);
    }
}

#[test]
fn a_commit_timestamp_with_z_or_with_a_malformed_shape_renders_as_the_rule_says() {
    let zulu = CommitHit {
        timestamp: "2026-09-22T14:03:11Z".to_owned(),
        message: "Fix".to_owned(),
        paths: Vec::new(),
        ..commit_hit()
    };
    let malformed = CommitHit {
        timestamp: "last tuesday".to_owned(),
        ..zulu.clone()
    };
    golden(
        &only_page(vec![commit_target(zulu), commit_target(malformed)]),
        &[
            "2 results",
            "\t[1] 1f2080e4 · Alice <alice@example.com> · 2026-09-22 14:03 Z",
            "\t\tFix",
            "\t[2] 1f2080e4 · Alice <alice@example.com> · last tuesday",
            "\t\tFix",
        ],
    );
}

#[test]
fn only_a_lowercase_hex_value_of_eight_characters_or_more_is_cut_to_eight() {
    assert_eq!(
        cut_hash("1f2080e49da12fee4431e6872630509355cd62d1"),
        "1f2080e4"
    );
    assert_eq!(cut_hash("1f2080e4"), "1f2080e4");
    assert_eq!(cut_hash("1f2080e"), "1f2080e");
    assert_eq!(cut_hash("main"), "main");
    assert_eq!(cut_hash("main^2"), "main^2");
    assert_eq!(cut_hash("1F2080E49DA1"), "1F2080E49DA1");
    assert_eq!(cut_hash("1f2080e49dag"), "1f2080e49dag");
    assert_eq!(
        cut_hash("rift://symbol/rust/a.rs/A"),
        "rift://symbol/rust/a.rs/A"
    );
    assert_eq!(cut_hash(""), "");
}

#[test]
fn a_revision_that_is_not_hex_is_written_unchanged_and_a_hex_one_is_cut() {
    let branch = CommitHit {
        revision: RevisionId("release/v1.2".to_owned()),
        message: "Fix".to_owned(),
        paths: Vec::new(),
        ..commit_hit()
    };
    let hex = CommitHit {
        revision: RevisionId("0123456789abcdef".to_owned()),
        ..branch.clone()
    };
    let output = rendered(&only_page(vec![commit_target(branch), commit_target(hex)]));
    assert!(output.contains("[1] release/v1.2 · "), "{output}");
    assert!(output.contains("[2] 01234567 · "), "{output}");
}

#[test]
fn the_score_of_a_commit_hit_ends_its_first_line() {
    let hit = SearchHit {
        score: Some(0.5),
        ..commit_target(CommitHit {
            message: "Fix".to_owned(),
            paths: Vec::new(),
            ..commit_hit()
        })
    };
    golden(
        &only_page(vec![hit]),
        &[
            "1 result",
            "\t1f2080e4 · Alice <alice@example.com> · 2026-08-21 14:03 +00:00 · score 0.5",
            "\tFix",
        ],
    );
}

// Control characters and source bytes.

#[test]
fn control_characters_in_single_line_facts_are_written_as_escapes() {
    let mut symbol = signed_symbol(
        Some("rift://symbol/rust/a.rs/A"),
        "A",
        "fn a(\n\tx: u8)\r\u{1b}\u{7f}",
    );
    symbol.documentation = vec![documentation_text("\u{0}first\u{8}\nsecond")];
    let hit = SearchHit {
        path: Some(project_path("dir\ttab/a.rs")),
        ..placed_symbol_hit(symbol)
    };
    let commit = CommitHit {
        message: "Subject\r\n\u{1b}[31m\n\nBody keeps\ttabs".to_owned(),
        author: CommitAuthor {
            name: "Al\nice".to_owned(),
            email: "a\u{85}@example.com".to_owned(),
        },
        paths: vec![project_path("p\nq")],
        ..commit_hit()
    };
    let output = rendered(&only_page(vec![hit, commit_target(commit)]));
    assert_no_trailing_space(&output);
    assert!(
        output.contains("\n\t[1] fn a(\\n\\tx: u8)\\r\\u{1b}\\u{7f}\n"),
        "{output}"
    );
    assert!(
        output.contains("\n\t\tdir\\ttab/a.rs:1 · name\n"),
        "{output}"
    );
    assert!(output.contains("\n\t\t\\u{0}first\\u{8}\n"), "{output}");
    assert!(
        output.contains("Al\\nice <a\\u{85}@example.com>"),
        "{output}"
    );
    assert!(output.contains("\n\t\tSubject\\r\n"), "{output}");
    assert!(output.contains("\n\t\t\tp\\nq\n"), "{output}");
    assert!(output.contains("\n\t\tBody keeps\ttabs\n"), "{output}");
}

#[test]
fn multi_line_source_keeps_its_bytes() {
    let source = "fn a() {\n\tlet x = \"q\";\r\n\n    // tail  \n}\n\n";
    let hit = SearchHit {
        source: Some(source.to_owned()),
        ..placed_symbol_hit(symbol(Some(A_ID), "A"))
    };
    let output = rendered(&only_page(vec![hit]));
    let prefix = "1 result\n\tstruct A\n\ta.rs:1 · name\n\trift://symbol/rust/a.rs/A\n\n";
    let written = output
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix('\n'))
        .expect("source closes the single item");
    assert_eq!(unindented(written), source);
}

/// `written` with the one-level indent of a single item removed from every non-empty line.
fn unindented(written: &str) -> String {
    let lines: Vec<&str> = written
        .split('\n')
        .map(|line| {
            let unindented = line.strip_prefix("\t");
            assert!(
                unindented.is_some() || line.is_empty(),
                "a source line lost its indent: {line:?}"
            );
            unindented.unwrap_or(line)
        })
        .collect();
    lines.join("\n")
}

#[test]
fn multi_line_source_of_several_items_loses_only_the_two_level_indent() {
    let source = "a\n\n  b\n";
    let items: Vec<SearchHit> = (0..2)
        .map(|_| SearchHit {
            source: Some(source.to_owned()),
            ..search_hit(SearchHitTarget::File {
                size: 1,
                languages: Vec::new(),
            })
        })
        .collect();
    golden(
        &only_page(items),
        &[
            "2 results",
            "\t[1]",
            "\t\ta",
            "",
            "\t\t  b",
            "",
            "\t[2]",
            "\t\ta",
            "",
            "\t\t  b",
            "",
        ],
    );
}

// Wire names.

#[test]
fn wire_names_are_the_serde_spellings_of_unit_variants() {
    let every = [
        MatchedField::Name,
        MatchedField::Signature,
        MatchedField::Documentation,
        MatchedField::Content,
        MatchedField::Ranked,
        MatchedField::Path,
        MatchedField::Relationship,
        MatchedField::Change,
    ];
    assert_eq!(
        wire_names(&every).as_deref(),
        Ok("name, signature, documentation, content, ranked, path, relationship, change")
    );
    assert_eq!(wire_names::<MatchedField>(&[]).as_deref(), Ok(""));
    assert_eq!(
        spaced_name(&SymbolVersionKind::DecoratorsChanged).as_deref(),
        Ok("decorators changed")
    );
    assert_eq!(
        wire_name(&SourceLocationKind::Stdlib).as_deref(),
        Ok("stdlib")
    );
    for not_a_name in [wire_name(&7_u8), wire_name(&Some(1_u8)), wire_name(&["a"])] {
        assert_eq!(
            not_a_name,
            Err(TextError::Unsupported("enum without a unit wire name"))
        );
    }
}

#[test]
fn one_path_is_counted_in_the_singular() {
    let commit = CommitHit {
        message: "Fix".to_owned(),
        paths: vec![project_path("src/a.rs")],
        ..commit_hit()
    };
    golden(
        &commit_answer(commit),
        &[
            "1 result",
            "\t1f2080e4 · Alice <alice@example.com> · 2026-08-21 14:03 +00:00",
            "\tFix",
            "",
            "\t1 path:",
            "\t\tsrc/a.rs",
        ],
    );
}

// get_symbol answers.

#[test]
fn an_empty_get_symbol_answer_writes_the_header_alone() {
    golden(&symbol_result(Vec::new(), pagination(0, 0)), &["0 results"]);
}

/// The authored `get_symbol` example, which the owner's example matches.
#[test]
fn the_owner_get_symbol_example_with_documentation_history_and_source() {
    golden(
        &example::<GetSymbolResult>(0),
        &[
            "1 result",
            "\tpub fn load_config(path: &Path) -> Result<Config, ConfigError>",
            "\tsrc/config.rs:10",
            "\trift://symbol/rust/src/config.rs/load_config",
            "",
            "\tLoads the workspace configuration from `rift.toml`.",
            "",
            "\thistory:",
            "\t\t2026-08-21 · 1f2080e4 · signature changed · Alice <alice@example.com>",
            "\t\t\tReturn ConfigError from load_config",
            "\t\t2026-08-17 · 82590265 · introduced · Alice <alice@example.com>",
            "\t\t\tAdd workspace configuration loading",
            "",
            "\t/// Loads the workspace configuration from `rift.toml`.",
            "\tpub fn load_config(path: &Path) -> Result<Config, ConfigError> {",
            "\t    let text = std::fs::read_to_string(path)?;",
            "\t    parse_config(&text)",
            "\t}",
        ],
    );
}

#[test]
fn a_hit_without_source_ends_after_its_last_section_and_an_empty_source_adds_an_empty_line() {
    golden(
        &symbol_result(vec![symbol_hit_with_source(None)], pagination(0, 1)),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
        ],
    );
    golden(
        &symbol_result(vec![symbol_hit_with_source(Some(""))], pagination(0, 1)),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "",
            "",
        ],
    );
    golden(
        &symbol_result(
            vec![symbol_hit_with_source(Some("struct A;"))],
            pagination(0, 1),
        ),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\tstruct A;",
        ],
    );
}

#[test]
fn page_and_warnings_of_a_get_symbol_answer_follow_the_shared_header() {
    let mut answer = symbol_result(vec![symbol_hit_with_source(None)], pagination(1, 4));
    answer.warnings = vec![ReadWarning::GlobalAccessDisabled];
    golden(
        &answer,
        &[
            "1 result · page 2/4",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "1 warning",
            "\tglobal_access_disabled",
        ],
    );
}

#[test]
fn a_symbol_without_an_identity_shows_its_range_and_a_dependency_hit_its_unit() {
    let mut anonymous = symbol_hit_with_source(None);
    anonymous.symbol.id = None;
    let mut dependency = symbol_hit_with_source(None);
    dependency.path = None;
    dependency.unit = Some(source_unit("rift://source/cargo/serde/lib.rs"));
    dependency.line = 3;
    golden(
        &symbol_result(vec![anonymous, dependency], pagination(0, 1)),
        &[
            "2 results",
            "\t[1] struct A",
            "\t\ta.rs:1",
            "\t\t0..9",
            "\t[2] struct A",
            "\t\trift://source/cargo/serde/lib.rs:3",
            "\t\trift://symbol/rust/a.rs/A",
        ],
    );
}

#[test]
fn exceptional_facts_of_a_get_symbol_hit_share_one_line_without_score_and_distance() {
    let mut hit = symbol_hit_with_source(None);
    hit.symbol.origin = origin(
        Some(SourceLocationKind::Dependency),
        Some(package("serde", "1.0.0")),
        SourceKind::Generated,
    );
    hit.symbol.facets = vec![SymbolFacet::Deprecated, SymbolFacet::Test];
    hit.symbol.document_local = true;
    hit.symbol.visibility = Some("pub".to_owned());
    golden(
        &symbol_result(vec![hit], pagination(0, 1)),
        &[
            "1 result",
            "\tpub struct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "\tdependency serde@1.0.0 · generated · deprecated · test · document local",
        ],
    );
}

#[test]
fn every_documentation_text_follows_in_order_and_blank_texts_are_skipped() {
    let mut hit = symbol_hit_with_source(None);
    hit.symbol.documentation = vec![
        documentation_text("First line.\nSecond line."),
        documentation_text(" \n"),
        documentation_text(""),
        documentation_text("Other text."),
    ];
    golden(
        &symbol_result(vec![hit], pagination(0, 1)),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\tFirst line.",
            "\tSecond line.",
            "",
            "\tOther text.",
        ],
    );
}

fn context(references: &serde_json::Value, truncated: bool) -> DocumentationContext {
    serde_json::from_value(json!({
        "documentation_revision": "3f9a1c2e",
        "references": references,
        "truncated": truncated
    }))
    .expect("documentation context deserializes")
}

fn reference(hit: &DocumentationHit, excerpt: Option<&str>) -> serde_json::Value {
    let mut value = json!({
        "reference": {
            "identity": "e".repeat(64),
            "block": "a".repeat(64),
            "target": A_ID,
            "range": {"start": 0, "end": 1},
            "authored": "A",
            "evidence": "provider"
        },
        "documentation": hit
    });
    if let Some(excerpt) = excerpt {
        value["excerpt"] = json!(excerpt);
    }
    value
}

#[test]
fn the_documentation_context_lists_each_reference_with_its_excerpt() {
    let guide = documentation_hit_at("docs/guide.md", 12, &["Guide", "Install"], None);
    let readme = documentation_hit_at("README.md", 3, &[], None);
    let references = json!([
        reference(&guide, Some("Use `A`.\n\nThen run it.")),
        reference(&readme, None),
        reference(&readme, Some("")),
    ]);
    let mut hit = symbol_hit_with_source(Some("struct A;"));
    hit.documentation = Some(context(&references, false));
    golden(
        &symbol_result(vec![hit], pagination(0, 1)),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\tdocumentation:",
            "\t\tdocs/guide.md:12 · Guide > Install",
            "\t\t\tUse `A`.",
            "",
            "\t\t\tThen run it.",
            "\t\tREADME.md:3",
            "\t\tREADME.md:3",
            "",
            "\tstruct A;",
        ],
    );
}

#[test]
fn a_truncated_or_empty_documentation_context_keeps_its_label() {
    let mut truncated = symbol_hit_with_source(None);
    truncated.documentation = Some(context(&json!([]), true));
    let mut empty = symbol_hit_with_source(None);
    empty.documentation = Some(context(&json!([]), false));
    golden(
        &symbol_result(vec![truncated, empty], pagination(0, 1)),
        &[
            "2 results",
            "\t[1] struct A",
            "\t\ta.rs:1",
            "\t\trift://symbol/rust/a.rs/A",
            "",
            "\t\tdocumentation (truncated):",
            "\t[2] struct A",
            "\t\ta.rs:1",
            "\t\trift://symbol/rust/a.rs/A",
            "",
            "\t\tdocumentation:",
        ],
    );
}

#[test]
fn documentation_context_warnings_follow_the_references_as_documentation_warning_lines() {
    let guide = documentation_hit_at("docs/guide.md", 2, &["Guide"], None);
    let mut context = context(&json!([reference(&guide, None)]), false);
    let warning = json!({
        "source": {"source": {"kind": "project", "path": "docs/a.md"}},
        "stage": "extract", "kind": "malformed_source", "count": 3});
    let truncated = json!({
        "source": {"source": {"kind": "project", "path": "docs/b.md"}},
        "stage": "source", "kind": "source_truncated", "count": 1});
    context.warnings = vec![
        serde_json::from_value(warning.clone()).expect("documentation warning deserializes"),
        serde_json::from_value(truncated).expect("documentation warning deserializes"),
    ];
    let mut hit = symbol_hit_with_source(None);
    hit.documentation = Some(context);
    golden(
        &symbol_result(vec![hit], pagination(0, 1)),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\tdocumentation:",
            "\t\tdocs/guide.md:2 · Guide",
            "\t\tdocumentation · warning (source docs/a.md, stage extract, kind malformed_source, count 3)",
            "\t\tdocumentation · warning (source docs/b.md, stage source, kind source_truncated, count 1)",
        ],
    );
    let read_warning = json!({"code": "documentation", "warning": warning});
    let shared_line = warning_line(&read_warning);
    let context_line =
        "documentation · warning (source docs/a.md, stage extract, kind malformed_source, count 3)";
    assert_eq!(
        shared_line, context_line,
        "one line for one warning wherever it is read"
    );
}

#[test]
fn an_empty_documentation_context_with_a_warning_writes_the_label_and_the_warning() {
    let mut context = context(&json!([]), true);
    context.warnings = vec![
        serde_json::from_value(json!({
            "source": {"source": {"kind": "project", "path": "docs/a.md"}},
            "stage": "source", "kind": "source_unavailable", "count": 2}))
        .expect("documentation warning deserializes"),
    ];
    let mut hit = symbol_hit_with_source(None);
    hit.documentation = Some(context);
    golden(
        &symbol_result(vec![hit], pagination(0, 1)),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\tdocumentation (truncated):",
            "\t\tdocumentation · warning (source docs/a.md, stage source, kind source_unavailable, count 2)",
        ],
    );
}

#[test]
fn a_get_symbol_value_holding_a_delimiter_is_quoted_and_others_stay_bare() {
    let guide = documentation_hit_at("docs/guide.md", 2, &["Step 1: Install", "a · b"], None);
    let mut hit = symbol_hit_with_source(None);
    hit.documentation = Some(context(&json!([reference(&guide, None)]), false));
    let mut moved = version("aaaaaaaa11", "src/a · b.rs", SymbolVersionKind::Moved, None);
    moved.author = CommitAuthor {
        name: "Ops: \"bot\" \\ team".to_owned(),
        email: "ops@example.com".to_owned(),
    };
    let mut odd_time = version("bbbbbbbb22", "a.rs", SymbolVersionKind::Introduced, None);
    odd_time.timestamp = "day 1: noon".to_owned();
    hit.history = Some(history(vec![moved, odd_time], true));
    golden(
        &symbol_result(vec![hit], pagination(0, 1)),
        &[
            "1 result",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\tdocumentation:",
            "\t\tdocs/guide.md:2 · \"Step 1: Install > a · b\"",
            "",
            "\thistory:",
            "\t\t2026-08-21 · aaaaaaaa · moved · \"Ops: \\\"bot\\\" \\\\ team <ops@example.com>\" · \"src/a · b.rs\"",
            "\t\t\"day 1: noon\" · bbbbbbbb · introduced · Alice <alice@example.com>",
        ],
    );
}

#[test]
fn a_search_value_holding_a_delimiter_is_quoted_and_others_stay_bare() {
    let mut moved = changed(
        SymbolVersionKind::Moved,
        Some("old: a.rs"),
        Some("new · a.rs"),
    );
    moved.path = Some(project_path("src/a · b.rs"));
    let mut bare = placed_symbol_hit(symbol(Some(A_ID), "A"));
    bare.path = Some(project_path("src/a:b·c.rs"));
    let commit = CommitHit {
        message: "Fix".to_owned(),
        author: CommitAuthor {
            name: "Ops: bot".to_owned(),
            email: "ops@example.com".to_owned(),
        },
        timestamp: "day 1 · noon".to_owned(),
        paths: Vec::new(),
        ..commit_hit()
    };
    golden(
        &only_page(vec![moved, bare, commit_target(commit)]),
        &[
            "3 results",
            "\t[1] struct A",
            "\t\t\"src/a · b.rs:1\" · name",
            "\t\trift://symbol/rust/a.rs/A",
            "\t\tmoved · \"old: a.rs\" → \"new · a.rs\"",
            "\t[2] struct A",
            "\t\tsrc/a:b·c.rs:1 · name",
            "\t\trift://symbol/rust/a.rs/A",
            "\t[3] 1f2080e4 · \"Ops: bot <ops@example.com>\" · \"day 1 · noon\"",
            "\t\tFix",
        ],
    );
}

#[test]
fn a_node_kind_holding_a_delimiter_is_quoted_and_others_stay_bare() {
    let answer = nodes_result(
        vec![
            node("rift://node/rust/lib.rs@0-9#aaaaaaaa", "a · b", 0, 9),
            node("rift://node/rust/lib.rs@1-8#bbbbbbbb", "c: d", 1, 8),
            node("rift://node/rust/lib.rs@2-7#cccccccc", "e:f·g", 2, 7),
        ],
        vec!["a", "b", "c"],
    );
    golden(
        &answer,
        &[
            "3 nodes",
            "\t[1] \"a · b\"",
            "\t\trift://node/rust/lib.rs@0-9#aaaaaaaa",
            "\t[2] \"c: d\"",
            "\t\trift://node/rust/lib.rs@1-8#bbbbbbbb",
            "\t[3] e:f·g",
            "\t\trift://node/rust/lib.rs@2-7#cccccccc",
            "",
            "\t\tc",
        ],
    );
}

#[test]
fn a_map_value_holding_a_delimiter_is_quoted_and_others_stay_bare() {
    let map: WorkspaceMap = serde_json::from_value(json!({
        "revision": "3f9a1c2e",
        "modules": [
            {"path": "src · a", "files": 1, "symbols": 1, "children": [
                {"path": "src · a/b: c", "files": 1, "symbols": 1}
            ]},
            {"path": "a:b·c", "files": 1, "symbols": 1}
        ],
        "hubs": [{"symbol": A_ID, "kind": "x · y", "references": 1}],
        "module_relationships": [{"from": "a: b", "to": "a:b·c", "references": 2}],
        "pagination": {"page_index": 0, "total_pages": 1}
    }))
    .expect("the map fixture deserializes");
    golden(
        &map,
        &[
            "map 3f9a1c2e",
            "modules:",
            "\t\"src · a\" · 1 file · 1 symbol",
            "\t\t\"b: c\" · 1 file · 1 symbol",
            "\ta:b·c · 1 file · 1 symbol",
            "hubs:",
            "\trift://symbol/rust/a.rs/A · \"x · y\" · 1 reference",
            "module relationships:",
            "\t\"a: b\" → a:b·c · 2 references",
        ],
    );
}

#[test]
fn a_workspace_value_holding_a_delimiter_is_quoted_and_others_stay_bare() {
    let page: WorkspaceResourcePage = serde_json::from_value(json!({
        "configuration_revision": "3f9a1c2e",
        "languages": [{"language": "python", "enabled": true, "execution": false,
            "syntax": false, "include": ["src/a · b/**"], "exclude": ["docs: old/**", "a:b·c/**"]}],
        "source": [
            {"path": "src/a · b.py", "digest": "8a4d20bc", "language": "python"},
            {"path": "x: y.py", "digest": "8a4d20bc"},
            {"path": "a:b·c.py", "digest": "8a4d20bc"}
        ],
        "pagination": {"page_index": 0, "total_pages": 1}
    }))
    .expect("the workspace fixture deserializes");
    golden(
        &page,
        &[
            "workspace 3f9a1c2e",
            "languages:",
            "\tpython",
            "\t\tinclude \"src/a · b/**\"",
            "\t\texclude \"docs: old/**\", a:b·c/**",
            "source:",
            "\t\"src/a · b.py\" · python · 8a4d20bc",
            "\t\"x: y.py\" · 8a4d20bc",
            "\ta:b·c.py · 8a4d20bc",
        ],
    );
}

fn version(
    revision: &str,
    path: &str,
    kind: SymbolVersionKind,
    summary: Option<&str>,
) -> SymbolVersion {
    SymbolVersion {
        revision: RevisionId(revision.to_owned()),
        path: project_path(path),
        kind,
        timestamp: "2026-08-21T14:03:22+00:00".to_owned(),
        summary: summary.map(str::to_owned),
        author: author(),
    }
}

fn history(versions: Vec<SymbolVersion>, complete: bool) -> SymbolHistory {
    SymbolHistory {
        symbol: SymbolId(A_ID.to_owned()),
        versions,
        complete,
    }
}

#[test]
fn history_lists_versions_and_marks_an_incomplete_timeline() {
    let mut complete = symbol_hit_with_source(None);
    complete.history = Some(history(
        vec![
            version(
                "1f2080e49da12fee",
                "a.rs",
                SymbolVersionKind::BodyChanged,
                Some("Fix\nit"),
            ),
            version("HEAD~2", "a.rs", SymbolVersionKind::Introduced, None),
        ],
        true,
    ));
    let mut incomplete = symbol_hit_with_source(None);
    incomplete.history = Some(history(Vec::new(), false));
    golden(
        &symbol_result(vec![complete, incomplete], pagination(0, 1)),
        &[
            "2 results",
            "\t[1] struct A",
            "\t\ta.rs:1",
            "\t\trift://symbol/rust/a.rs/A",
            "",
            "\t\thistory:",
            "\t\t\t2026-08-21 · 1f2080e4 · body changed · Alice <alice@example.com>",
            "\t\t\t\tFix\\nit",
            "\t\t\t2026-08-21 · HEAD~2 · introduced · Alice <alice@example.com>",
            "\t[2] struct A",
            "\t\ta.rs:1",
            "\t\trift://symbol/rust/a.rs/A",
            "",
            "\t\thistory (incomplete):",
        ],
    );
}

#[test]
fn a_version_names_its_path_only_when_it_differs_from_the_hit() {
    let mut hit = symbol_hit_with_source(None);
    hit.history = Some(history(
        vec![
            version("aaaaaaaa11", "b/a.rs", SymbolVersionKind::Moved, None),
            version("bbbbbbbb22", "a.rs", SymbolVersionKind::Introduced, None),
        ],
        true,
    ));
    let mut dependency = symbol_hit_with_source(None);
    dependency.path = None;
    dependency.unit = Some(source_unit("rift://source/cargo/serde/lib.rs"));
    dependency.history = Some(history(
        vec![version(
            "cccccccc33",
            "a.rs",
            SymbolVersionKind::Removed,
            None,
        )],
        true,
    ));
    let output = rendered(&symbol_result(vec![hit, dependency], pagination(0, 1)));
    assert_no_trailing_space(&output);
    assert!(
        output.contains("aaaaaaaa · moved · Alice <alice@example.com> · b/a.rs\n"),
        "{output}"
    );
    assert!(
        output.contains("bbbbbbbb · introduced · Alice <alice@example.com>\n"),
        "{output}"
    );
    assert!(
        output.contains("cccccccc · removed · Alice <alice@example.com> · a.rs\n"),
        "{output}"
    );
}

#[test]
fn sections_come_in_order_documentation_text_context_history_source() {
    let guide = documentation_hit_at("docs/guide.md", 2, &["Guide"], None);
    let mut hit = symbol_hit_with_source(Some("struct A;\n"));
    hit.symbol.documentation = vec![documentation_text("Text.")];
    hit.documentation = Some(context(&json!([reference(&guide, None)]), false));
    hit.history = Some(history(
        vec![version(
            "1f2080e49d",
            "a.rs",
            SymbolVersionKind::Introduced,
            None,
        )],
        true,
    ));
    let output = rendered(&symbol_result(vec![hit], pagination(0, 1)));
    assert_no_trailing_space(&output);
    assert_eq!(
        output,
        text(&[
            "1 result",
            "\tstruct A",
            "\ta.rs:1",
            "\trift://symbol/rust/a.rs/A",
            "",
            "\tText.",
            "",
            "\tdocumentation:",
            "\t\tdocs/guide.md:2 · Guide",
            "",
            "\thistory:",
            "\t\t2026-08-21 · 1f2080e4 · introduced · Alice <alice@example.com>",
            "",
            "\tstruct A;",
            "",
        ])
    );
}

#[test]
fn several_get_symbol_hits_have_markers_and_indent() {
    let mut first = symbol_hit_with_source(Some("struct A {\n\n    x: u8,\n}"));
    first.symbol.documentation = vec![documentation_text("Doc.")];
    let mut second = symbol_hit_with_source(Some("struct B;"));
    second.symbol = signed_symbol(Some("rift://symbol/rust/b.rs/B"), "B", "struct B;");
    second.path = Some(project_path("b.rs"));
    golden(
        &symbol_result(vec![first, second], pagination(0, 1)),
        &[
            "2 results",
            "\t[1] struct A",
            "\t\ta.rs:1",
            "\t\trift://symbol/rust/a.rs/A",
            "",
            "\t\tDoc.",
            "",
            "\t\tstruct A {",
            "",
            "\t\t    x: u8,",
            "\t\t}",
            "\t[2] struct B;",
            "\t\tb.rs:1",
            "\t\trift://symbol/rust/b.rs/B",
            "",
            "\t\tstruct B;",
        ],
    );
}

#[test]
fn control_characters_in_get_symbol_facts_are_escaped_and_source_keeps_its_bytes() {
    let source = "fn a() {\n\t\"q\"\r\n}\n\n";
    let mut hit = symbol_hit_with_source(Some(source));
    hit.symbol = signed_symbol(Some(A_ID), "A", "fn a()\n\u{1b}");
    hit.history = Some(history(
        vec![version(
            "1f2080e49d",
            "a.rs",
            SymbolVersionKind::Introduced,
            Some("a\tb"),
        )],
        true,
    ));
    let output = rendered(&symbol_result(vec![hit], pagination(0, 1)));
    assert!(output.contains("\n\tfn a()\\n\\u{1b}\n"), "{output}");
    assert!(output.contains("\n\t\t\ta\\tb\n"), "{output}");
    let written = output
        .split_once("\n\n\tfn a() {")
        .map(|(_, rest)| format!("\tfn a() {{{rest}"))
        .and_then(|rest| rest.strip_suffix('\n').map(str::to_owned))
        .expect("source closes the single hit");
    assert_eq!(unindented(&written), source);
}

// Nodes answers.

fn outer_and_inner() -> NodesResult {
    let outer = node(
        "rift://node/rust/lib.rs@0-20#aaaaaaaa",
        "function_item",
        0,
        20,
    );
    let mut inner = node("rift://node/rust/lib.rs@3-9#bbbbbbbb", "identifier", 3, 9);
    inner.parent = Some(outer.id.clone());
    nodes_result(vec![outer, inner], vec!["fn name() {}\n// tail", "name"])
}

#[test]
fn an_empty_nodes_answer_writes_only_the_header() {
    let answer = nodes_result(Vec::new(), Vec::new());
    golden(&answer, &["0 nodes"]);
}

#[test]
fn one_node_has_no_marker_and_its_excerpt_follows_a_blank_line() {
    let answer = nodes_result(
        vec![node(
            "rift://node/rust/lib.rs@3-9#bbbbbbbb",
            "identifier",
            3,
            9,
        )],
        vec!["name"],
    );
    golden(
        &answer,
        &[
            "1 node",
            "\tidentifier",
            "\trift://node/rust/lib.rs@3-9#bbbbbbbb",
            "",
            "\tname",
        ],
    );
}

#[test]
fn several_nodes_follow_each_other_without_a_blank_line_and_only_the_innermost_excerpt_is_written()
{
    let answer = outer_and_inner();
    golden(
        &answer,
        &[
            "2 nodes",
            "\t[1] function_item · 2 lines",
            "\t\trift://node/rust/lib.rs@0-20#aaaaaaaa",
            "\t[2] identifier",
            "\t\trift://node/rust/lib.rs@3-9#bbbbbbbb",
            "",
            "\t\tname",
        ],
    );
    assert!(
        !rendered(&answer).contains("tail"),
        "the outer excerpt is not written"
    );
}

#[test]
fn a_node_names_its_symbol_its_regions_and_its_generated_and_test_facets() {
    let mut item = node(
        "rift://node/rust/lib.rs@0-20#aaaaaaaa",
        "function_item",
        0,
        20,
    );
    item.symbol = Some(SymbolId(A_ID.to_owned()));
    item.facets = vec![
        NodeFacet::Declaration,
        NodeFacet::Test,
        NodeFacet::Definition,
        NodeFacet::Generated,
    ];
    item.regions = vec![
        NodeRegion {
            role: RegionRole::Name,
            range: range(3, 4),
        },
        NodeRegion {
            role: RegionRole::Body,
            range: range(8, 20),
        },
    ];
    let mut plain = node("rift://node/rust/lib.rs@9-10#bbbbbbbb", "block", 9, 10);
    plain.facets = vec![NodeFacet::Body, NodeFacet::Block];
    golden(
        &nodes_result(vec![item, plain], vec!["fn a() {\n    b\n}", "{"]),
        &[
            "2 nodes",
            "\t[1] function_item · 3 lines · generated · test",
            "\t\trift://node/rust/lib.rs@0-20#aaaaaaaa",
            "\t\trift://symbol/rust/a.rs/A",
            "\t\tname 3..4 · body 8..20",
            "\t[2] block",
            "\t\trift://node/rust/lib.rs@9-10#bbbbbbbb",
            "",
            "\t\t{",
        ],
    );
}

#[test]
fn the_line_count_follows_the_excerpt_lines_and_not_its_trailing_line_feed() {
    let nodes = vec![
        node("rift://node/rust/lib.rs@0-1#aaaaaaaa", "a", 0, 1),
        node("rift://node/rust/lib.rs@1-2#bbbbbbbb", "b", 1, 2),
        node("rift://node/rust/lib.rs@2-3#cccccccc", "c", 2, 3),
        node("rift://node/rust/lib.rs@3-4#dddddddd", "d", 3, 4),
    ];
    golden(
        &nodes_result(nodes, vec!["", "x\n", "x\ny\n", "z"]),
        &[
            "4 nodes",
            "\t[1] a",
            "\t\trift://node/rust/lib.rs@0-1#aaaaaaaa",
            "\t[2] b",
            "\t\trift://node/rust/lib.rs@1-2#bbbbbbbb",
            "\t[3] c · 2 lines",
            "\t\trift://node/rust/lib.rs@2-3#cccccccc",
            "\t[4] d",
            "\t\trift://node/rust/lib.rs@3-4#dddddddd",
            "",
            "\t\tz",
        ],
    );
}

#[test]
fn an_empty_innermost_excerpt_is_one_empty_line_after_the_blank_line() {
    let answer = nodes_result(
        vec![node("rift://node/rust/lib.rs@0-1#aaaaaaaa", "a", 0, 1)],
        vec![""],
    );
    assert_eq!(
        rendered(&answer),
        text(&[
            "1 node",
            "\ta",
            "\trift://node/rust/lib.rs@0-1#aaaaaaaa",
            "",
            ""
        ])
    );
}

#[test]
fn warnings_follow_the_innermost_excerpt_of_the_nodes() {
    let mut answer = outer_and_inner();
    answer.warnings = vec![stale_index()];
    golden(
        &answer,
        &[
            "2 nodes",
            "\t[1] function_item · 2 lines",
            "\t\trift://node/rust/lib.rs@0-20#aaaaaaaa",
            "\t[2] identifier",
            "\t\trift://node/rust/lib.rs@3-9#bbbbbbbb",
            "",
            "\t\tname",
            "1 warning",
            "\tstale_index · index_tree_revision 3f9a1c2e · captured_tree_revision 3f9a1c2f: index lags the captured tree",
        ],
    );
}

#[test]
fn an_excerpt_with_backticks_and_header_lines_stays_verbatim_under_the_node() {
    let excerpt = "````rust\n```\n! stale_index\n2 nodes\n[1] fake\n\tnested\n````";
    let nodes = vec![node("rift://node/rust/lib.rs@0-9#aaaaaaaa", "a", 0, 9)];
    let single = nodes_result(nodes, vec![excerpt]);
    golden(
        &single,
        &[
            "1 node",
            "\ta · 7 lines",
            "\trift://node/rust/lib.rs@0-9#aaaaaaaa",
            "",
            "\t````rust",
            "\t```",
            "\t! stale_index",
            "\t2 nodes",
            "\t[1] fake",
            "\t\tnested",
            "\t````",
        ],
    );
    let two = nodes_result(
        vec![
            node("rift://node/rust/lib.rs@0-9#aaaaaaaa", "a", 0, 9),
            node("rift://node/rust/lib.rs@1-9#bbbbbbbb", "b", 1, 9),
        ],
        vec!["x", excerpt],
    );
    let output = rendered(&two);
    assert_no_trailing_space(&output);
    let block: Vec<&str> = output.lines().skip(6).collect();
    assert_eq!(block.len(), 7);
    let recovered: Vec<&str> = block
        .iter()
        .map(|line| line.strip_prefix("\t\t").unwrap_or(line))
        .collect();
    assert_eq!(recovered.join("\n"), excerpt);
}

#[test]
fn nodes_and_excerpts_that_differ_in_length_are_refused() {
    let one_node = || vec![node("rift://node/rust/lib.rs@0-1#aaaaaaaa", "a", 0, 1)];
    for answer in [
        nodes_result(one_node(), Vec::new()),
        nodes_result(Vec::new(), vec!["extra"]),
        nodes_result(one_node(), vec!["a", "b"]),
    ] {
        assert_eq!(
            text_of(&answer),
            Err(TextError::Unsupported("nodes and source differ in length"))
        );
    }
}

// Failures.

#[test]
fn a_failure_without_limit_or_causes_is_one_entry_with_its_message() {
    let error = error_data(ErrorCode::InvalidRequest, "expected `limit` of at least 1");
    golden(
        &error,
        &[
            "1 error",
            "\tinvalid_request · retry never",
            "\t\texpected `limit` of at least 1",
        ],
    );
}

#[test]
fn a_failure_writes_the_limit_and_each_cause_as_an_entry_with_its_code_and_directive() {
    let mut error = error_data(ErrorCode::LimitExceeded, "the request crosses a limit");
    error.limit = Some(LimitEvidence {
        field: "source.files".to_owned(),
        limit: 10,
        required: 11,
    });
    let cause = |code, message: &str, retry| ErrorCause {
        code,
        message: message.to_owned(),
        retry,
    };
    error.causes = vec![
        cause(
            ErrorCode::LimitExceeded,
            "the index build crossed it",
            RetryDirective::Never,
        ),
        cause(
            ErrorCode::LimitExceeded,
            "a retry may pass",
            RetryDirective::SameRequest,
        ),
        cause(
            ErrorCode::StorageFailure,
            "the store refused",
            RetryDirective::Never,
        ),
        cause(
            ErrorCode::InternalError,
            "task failed",
            RetryDirective::OperatorAction,
        ),
    ];
    golden(
        &error,
        &[
            "5 errors",
            "\tlimit_exceeded · retry never",
            "\t\tthe request crosses a limit",
            "\t\tlimit source.files: 11 over 10",
            "\tlimit_exceeded · retry never",
            "\t\tthe index build crossed it",
            "\tlimit_exceeded · retry same_request",
            "\t\ta retry may pass",
            "\tstorage_failure · retry never",
            "\t\tthe store refused",
            "\tinternal_error · retry operator_action",
            "\t\ttask failed",
        ],
    );
}

#[test]
fn a_failure_with_one_cause_and_no_limit_writes_two_entries_and_no_limit_line() {
    let mut error = error_data(ErrorCode::StorageFailure, "the store refused");
    error.retry = RetryDirective::SameRequest;
    error.causes = vec![ErrorCause {
        code: ErrorCode::StorageFailure,
        message: "disk full".to_owned(),
        retry: RetryDirective::SameRequest,
    }];
    golden(
        &error,
        &[
            "2 errors",
            "\tstorage_failure · retry same_request",
            "\t\tthe store refused",
            "\tstorage_failure · retry same_request",
            "\t\tdisk full",
        ],
    );
}

#[test]
fn a_failure_message_with_a_line_feed_stays_on_one_line() {
    let error = error_data(ErrorCode::InternalError, "first\nerror cancelled\t \u{1b}");
    golden(
        &error,
        &[
            "1 error",
            "\tinternal_error · retry never",
            "\t\tfirst\\nerror cancelled\\t \\u{1b}",
        ],
    );
}

#[test]
fn a_failure_with_an_empty_message_writes_its_head_and_no_message_entry() {
    golden(
        &error_data(ErrorCode::InternalError, ""),
        &["1 error", "\tinternal_error · retry never"],
    );
}

#[test]
fn a_registered_failure_writes_its_identity_after_its_limit_and_before_its_causes() {
    let mut error = error_data(
        ErrorCode::LimitExceeded,
        "the request crosses a limit; narrow the request",
    );
    error.limit = Some(LimitEvidence {
        field: "source.files".to_owned(),
        limit: 10,
        required: 11,
    });
    error.causes = vec![ErrorCause {
        code: ErrorCode::StorageFailure,
        message: "the store refused".to_owned(),
        retry: RetryDirective::SameRequest,
    }];
    golden(
        &RegisteredFailure {
            identity: "rift.index.workspace_too_many_files",
            error: &error,
        },
        &[
            "2 errors",
            "\tlimit_exceeded · retry never",
            "\t\tthe request crosses a limit; narrow the request",
            "\t\tlimit source.files: 11 over 10",
            "\t\trift.index.workspace_too_many_files",
            "\tstorage_failure · retry same_request",
            "\t\tthe store refused",
        ],
    );
}

#[test]
fn a_registered_failure_without_limit_or_causes_writes_its_identity_under_its_message() {
    let error = error_data(ErrorCode::InvalidRequest, "query is empty");
    golden(
        &RegisteredFailure {
            identity: "rift.ranking.query_empty",
            error: &error,
        },
        &[
            "1 error",
            "\tinvalid_request · retry never",
            "\t\tquery is empty",
            "\t\trift.ranking.query_empty",
        ],
    );
}

#[test]
fn a_registered_identity_with_a_control_character_stays_on_one_line() {
    let error = error_data(ErrorCode::InternalError, "first");
    golden(
        &RegisteredFailure {
            identity: "rift.test\nsplit",
            error: &error,
        },
        &[
            "1 error",
            "\tinternal_error · retry never",
            "\t\tfirst",
            "\t\trift.test\\nsplit",
        ],
    );
}

/// A diagnostic context with the given severity, code, span, and position.
fn diagnostic(
    severity: &str,
    code: Option<&str>,
    unit: Option<&str>,
    at: (u64, u64),
) -> DiagnosticContext {
    let mut finding = json!({
        "severity": severity, "message": "mismatched types\nexpected `u8`",
        "reliability": "reliable", "continuation": "repairable"
    });
    if let Some(code) = code {
        finding["code"] = json!(code);
    }
    if let Some(unit) = unit {
        finding["span"] = json!({"unit": unit, "range": {"start": 1, "end": 2}});
    }
    let mut context = json!({"source": "provider", "diagnostic": finding});
    if at.0 > 0 {
        context["line"] = json!(at.0);
    }
    if at.1 > 0 {
        context["column"] = json!(at.1);
    }
    serde_json::from_value(context).expect("diagnostic context deserializes")
}

#[test]
fn diagnostics_follow_the_message_and_the_limit_one_compiler_style_line_each() {
    let mut error = error_data(
        ErrorCode::CapabilityUnavailable,
        "the provider reported findings",
    );
    error.limit = Some(LimitEvidence {
        field: "diagnostics".to_owned(),
        limit: 4,
        required: 5,
    });
    error.causes = vec![ErrorCause {
        code: ErrorCode::CapabilityUnavailable,
        message: "no engine".to_owned(),
        retry: RetryDirective::Never,
    }];
    error.diagnostics = vec![
        diagnostic(
            "error",
            Some("E0308"),
            Some("rift://file/src/lib.rs"),
            (3, 9),
        ),
        diagnostic("warning", None, Some("rift://file/src/a%20b.rs"), (7, 0)),
        diagnostic("hint", None, Some("rift://file/src/lib.rs"), (0, 0)),
        diagnostic("info", Some("W1"), None, (0, 0)),
    ];
    golden(
        &error,
        &[
            "2 errors",
            "\tcapability_unavailable · retry never",
            "\t\tthe provider reported findings",
            "\t\tlimit diagnostics: 5 over 4",
            "\t\terror[E0308] src/lib.rs:3:9: mismatched types\\nexpected `u8`",
            "\t\twarning src/a%20b.rs:7: mismatched types\\nexpected `u8`",
            "\t\thint src/lib.rs: mismatched types\\nexpected `u8`",
            "\t\tinfo[W1]: mismatched types\\nexpected `u8`",
            "\tcapability_unavailable · retry never",
            "\t\tno engine",
        ],
    );
}

// Addresses.

#[test]
fn identities_are_written_whole_and_hashes_are_cut() {
    let symbol_id = "rift://symbol/rust/crates/rift-server/src/read.rs/ReadService~2";
    let node_id = "rift://node/rust/src/config.rs@218-355#67ecfb36";
    let unit = "rift://source/project/packages/@scope/name/package.json";
    let file = "rift://file/src/dir%20name/lib.rs";
    let mut hit = search_hit(symbol_target(Some(symbol_id), "ReadService"));
    hit.unit = Some(source_unit(unit));
    let search = rendered(&only_page(vec![hit]));
    let entry = |text: &str| format!("\t{text}");
    assert!(
        search.lines().any(|line| line == entry(symbol_id)),
        "{search}"
    );
    assert!(search.lines().any(|line| line == entry(unit)), "{search}");
    let mut symbols = symbol_hit_with_source(None);
    symbols.symbol.id = Some(SymbolId(symbol_id.to_owned()));
    symbols.node = Some(NodeId(node_id.to_owned()));
    let get_symbol = rendered(&symbol_result(vec![symbols], pagination(0, 1)));
    assert!(
        get_symbol.lines().any(|line| line == entry(symbol_id)),
        "{get_symbol}"
    );
    let mut node_item = node(node_id, "function_item", 0, 1);
    node_item.unit = FileId(file.to_owned());
    let nodes = rendered(&nodes_result(vec![node_item], vec!["x"]));
    assert!(nodes.lines().any(|line| line == entry(node_id)), "{nodes}");
    let mut commit = commit_hit();
    commit.revision = RevisionId("main^2".to_owned());
    let by_branch = rendered(&commit_answer(commit));
    assert!(by_branch.contains("\n\tmain^2 · "), "{by_branch}");
}

/// The authored `nodes` example, rendered unmodified.
#[test]
fn the_authored_nodes_example_renders_one_entry_per_node_and_the_innermost_source() {
    golden(
        &example::<NodesResult>(0),
        &[
            "5 nodes",
            "\t[1] source_file · 14 lines",
            "\t\trift://node/rust/src/config.rs@0-356#dcbef6dd",
            "\t[2] function_item · 4 lines",
            "\t\trift://node/rust/src/config.rs@218-355#67ecfb36",
            "\t\trift://symbol/rust/src/config.rs/load_config",
            "\t\tname 225..236 · body 281..355",
            "\t[3] block · 4 lines",
            "\t\trift://node/rust/src/config.rs@281-355#4e554fa8",
            "\t[4] call_expression",
            "\t\trift://node/rust/src/config.rs@334-353#4df4426e",
            "\t[5] identifier",
            "\t\trift://node/rust/src/config.rs@334-346#03f22dac",
            "",
            "\t\tparse_config",
        ],
    );
}

// Limits.

#[test]
fn an_answer_over_the_byte_limit_yields_no_text() {
    let answer = example::<SearchResult>(0);
    let full = rendered(&answer);
    let mut exact = TextWriter::new(full.len());
    answer.render(&mut exact).expect("exact fit renders");
    assert_eq!(exact.finish(), Ok(full.clone()));
    let mut tight = TextWriter::new(full.len() - 1);
    let failure = answer.render(&mut tight).expect_err("one byte over");
    assert_eq!(
        failure,
        TextError::Overflow(OutputOverflow {
            limit: full.len() - 1
        })
    );
    assert_eq!(tight.finish(), Err(failure));
}

/// Byte length stands in for model tokens: the repository carries no tokenizer, so this checks
/// bytes only, and the token measurement lives in the change report.
#[test]
fn the_compact_text_of_every_authored_example_is_shorter_in_bytes_than_its_json_text() {
    assert_text_shorter_than_json::<SearchResult>();
    assert_text_shorter_than_json::<GetSymbolResult>();
    assert_text_shorter_than_json::<NodesResult>();
}

// StatsAlloc is used only by this ignored measurement; run it alone in a release build.
#[global_allocator]
static ALLOCATOR: &stats_alloc::StatsAlloc<std::alloc::System> = &stats_alloc::INSTRUMENTED_SYSTEM;

/// Counts process-wide allocation and reallocation requests while `work` runs.
fn counted<R>(work: impl FnOnce() -> R) -> (stats_alloc::Stats, R) {
    let region = stats_alloc::Region::new(ALLOCATOR);
    let value = work();
    (region.change(), value)
}

#[derive(Clone, Copy)]
struct AllocationSample {
    answer: &'static str,
    example: u64,
    representation: &'static str,
    counts: stats_alloc::Stats,
    content_bytes: u64,
    structured_content_bytes: u64,
    combined_response_bytes: u64,
}

fn record_allocation_sample(sample: AllocationSample) {
    let AllocationSample {
        answer,
        example,
        representation,
        counts,
        content_bytes,
        structured_content_bytes,
        combined_response_bytes,
    } = sample;
    rift_tracing::info!(
        target: "rift_mcp::output::render_tests",
        measurement = "output allocation",
        answer,
        example,
        representation,
        allocations = u64::try_from(counts.allocations).expect("allocation count fits u64"),
        deallocations = u64::try_from(counts.deallocations).expect("deallocation count fits u64"),
        reallocations = u64::try_from(counts.reallocations).expect("reallocation count fits u64"),
        bytes_allocated = u64::try_from(counts.bytes_allocated).expect("allocation bytes fit u64"),
        bytes_deallocated =
            u64::try_from(counts.bytes_deallocated).expect("deallocation bytes fit u64"),
        bytes_reallocated =
            i64::try_from(counts.bytes_reallocated).expect("reallocation bytes fit i64"),
        content_bytes,
        structured_content_bytes,
        combined_response_bytes,
        "output allocation measurement"
    );
}

fn measure_allocations<T>(answer_name: &'static str)
where
    T: Render + Serialize + DeserializeOwned + JsonSchema + Clone,
{
    use rmcp::handler::server::tool::IntoCallToolResult as _;
    use rmcp::model::CallToolResponse;

    let answers = examples_of::<T>();
    let mut samples = Vec::with_capacity(answers.len() * 4);
    for (index, answer) in answers.into_iter().enumerate() {
        let example = u64::try_from(index).expect("example index fits u64");
        let (counts, text) = counted(|| rendered(&answer));
        samples.push(AllocationSample {
            answer: answer_name,
            example,
            representation: "text",
            counts,
            content_bytes: u64::try_from(text.len()).expect("content bytes fit u64"),
            structured_content_bytes: 0,
            combined_response_bytes: 0,
        });
        drop(text);

        let (counts, json) = counted(|| serde_json::to_string(&answer).expect("answer serializes"));
        samples.push(AllocationSample {
            answer: answer_name,
            example,
            representation: "json_string",
            counts,
            content_bytes: 0,
            structured_content_bytes: u64::try_from(json.len())
                .expect("structured content bytes fit u64"),
            combined_response_bytes: 0,
        });
        drop(json);

        let (counts, value) = counted(|| serde_json::to_value(&answer).expect("answer serializes"));
        let structured_content_bytes = serde_json::to_vec(&value)
            .expect("structured content serializes")
            .len();
        samples.push(AllocationSample {
            answer: answer_name,
            example,
            representation: "json_value",
            counts,
            content_bytes: 0,
            structured_content_bytes: u64::try_from(structured_content_bytes)
                .expect("structured content bytes fit u64"),
            combined_response_bytes: 0,
        });
        drop(value);

        let owned = answer.clone();
        let (counts, result) =
            counted(
                || match crate::output::Json(owned).into_call_tool_result() {
                    Ok(CallToolResponse::Complete(result)) => result,
                    _ => panic!("authored answer completes"),
                },
            );
        let content_bytes: usize = result
            .content
            .iter()
            .map(|content| {
                content
                    .as_text()
                    .expect("compact content is text")
                    .text
                    .len()
            })
            .sum();
        let structured_content = result
            .structured_content
            .as_ref()
            .expect("full output keeps structured content");
        let structured_content_bytes = serde_json::to_vec(structured_content)
            .expect("structured content serializes")
            .len();
        let combined_response_bytes = serde_json::to_vec(&result)
            .expect("tool result serializes")
            .len();
        samples.push(AllocationSample {
            answer: answer_name,
            example,
            representation: "call_tool_result",
            counts,
            content_bytes: u64::try_from(content_bytes).expect("content bytes fit u64"),
            structured_content_bytes: u64::try_from(structured_content_bytes)
                .expect("structured content bytes fit u64"),
            combined_response_bytes: u64::try_from(combined_response_bytes)
                .expect("response bytes fit u64"),
        });
        drop(result);
    }
    for sample in samples {
        record_allocation_sample(sample);
    }
}

#[test]
#[ignore = "an allocation measurement; run alone in a release build"]
fn output_allocation_cost() {
    // Trigger test-process OTLP setup before any allocation region begins.
    rift_tracing::info!(
        target: "rift_mcp::output::render_tests",
        measurement = "output allocation",
        "output allocation measurement started"
    );

    measure_allocations::<SearchResult>("search");
    measure_allocations::<GetSymbolResult>("get_symbol");
    measure_allocations::<NodesResult>("nodes");
}
