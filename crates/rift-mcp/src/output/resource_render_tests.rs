//! Goldens of the resource texts: the map, the workspace page, and the logs page.

use rift_protocol::dependencies::PackageAvailability;
use rift_protocol::map::WorkspaceMap;
use rift_protocol::workspace::WorkspaceResourcePage;
use serde_json::{Map, Value, json};

use super::facts::{CANONICAL_AVAILABILITY, package_text, wire_name};
use super::logs::utc_time;
use super::{LogFields, LogLine, LogsPage, Render, text_of};
use crate::output::text::{OutputOverflow, TextError, TextWriter};

/// The text of `answer`, with no line ending in a space.
fn rendered<T: Render>(answer: &T) -> String {
    let output = text_of(answer).expect("answer renders");
    for line in output.split('\n') {
        assert!(!line.ends_with(' '), "trailing space in {line:?}");
    }
    output
}

/// Asserts that `answer` renders as exactly `expected`, one entry per line.
fn golden<T: Render>(answer: &T, expected: &[&str]) {
    let mut text = expected.join("\n");
    text.push('\n');
    assert_eq!(rendered(answer), text);
}

fn map_of(value: Value) -> WorkspaceMap {
    serde_json::from_value(value).expect("the map fixture deserializes")
}

fn page_of(value: Value) -> WorkspaceResourcePage {
    serde_json::from_value(value).expect("the workspace fixture deserializes")
}

/// The response the protocol docs show for `rift://map`.
fn authored_map() -> WorkspaceMap {
    serde_json::from_str(
        r#"{
          "revision": "3f9a1c2e",
          "languages": [{ "language": "rust", "files": 191, "symbols": 3204 }],
          "modules": [{
            "path": "crates", "files": 191, "symbols": 3204,
            "children": [{ "path": "crates/rift-server", "files": 16, "symbols": 418 }]
          }],
          "entry_points": ["rift://symbol/rust/crates/rift/src/main.rs/main"],
          "docs": ["README.md"],
          "packages": [
            { "manager": "cargo", "name": "tokio", "version": "1.53.1", "availability": "canonical" },
            { "manager": "stdlib", "name": "rust", "version": "1.98.1", "availability": "canonical" }
          ],
          "pagination": { "page_index": 0, "total_pages": 1 }
        }"#,
    )
    .expect("the authored example deserializes")
}

#[test]
fn the_authored_map_example_renders_every_section_it_holds() {
    golden(
        &authored_map(),
        &[
            "map 3f9a1c2e",
            "languages:",
            "\trust · 191 files · 3204 symbols",
            "modules:",
            "\tcrates · 191 files · 3204 symbols",
            "\t\trift-server · 16 files · 418 symbols",
            "entry points:",
            "\trift://symbol/rust/crates/rift/src/main.rs/main",
            "docs:",
            "\tREADME.md",
            "packages:",
            "\ttokio@1.53.1 (cargo)",
            "\trust@1.98.1 (stdlib)",
        ],
    );
}

#[test]
fn a_full_map_renders_hubs_relationships_and_nested_modules() {
    let map = map_of(json!({
        "revision": "3f9a1c2e",
        "languages": [
            {"language": "markdown", "files": 1, "symbols": 1},
            {"language": "typescript:tsx", "files": 2, "symbols": 0}
        ],
        "modules": [
            {"path": "crates", "files": 9, "symbols": 80, "children": [
                {"path": "crates/rift-mcp", "files": 4, "symbols": 40, "children": [
                    {"path": "crates/rift-mcp/src", "files": 3, "symbols": 30, "children": [
                        {"path": "crates/rift-mcp/src/output", "files": 1, "symbols": 10}
                    ]},
                    {"path": "crates/rift-mcp/tests", "files": 1, "symbols": 10}
                ]},
                {"path": "crates/rift-server", "files": 5, "symbols": 40}
            ]},
            {"path": "crates-extra", "files": 1, "symbols": 1, "children": [
                {"path": "elsewhere", "files": 1, "symbols": 1},
                {"path": "crates-extra/sub", "files": 1, "symbols": 1}
            ]},
            {"path": "docs", "files": 0, "symbols": 0}
        ],
        "hubs": [
            {"symbol": "rift://symbol/rust/crates/rift-core/src/lib.rs/Revision",
             "kind": "struct", "references": 412},
            {"symbol": "rift://symbol/rust/crates/rift-core/src/lib.rs/Once",
             "kind": "function_item", "references": 1}
        ],
        "entry_points": [
            "rift://symbol/rust/crates/rift/src/main.rs/main",
            "rift://symbol/rust/crates/rift-cli/src/main.rs/cli"
        ],
        "docs": ["README.md", "docs/guide.md"],
        "module_relationships": [
            {"from": "crates/rift-mcp", "to": "crates/rift-server", "references": 312},
            {"from": "crates/rift-server", "to": "crates/rift-mcp", "references": 1}
        ],
        "packages": [
            {"manager": "cargo", "name": "serde", "requirement": "^1.0", "availability": "canonical"},
            {"manager": "cargo", "name": "tokio", "version": "1.53.1", "availability": "canonical"},
            {"manager": "cargo", "name": "local", "requirement": "*", "availability": "path"},
            {"manager": "npm", "name": "private", "version": "2.0.0", "availability": "private_registry"}
        ],
        "warnings": [
            {"code": "local_index_preparing", "prepared": 0, "total": 2,
             "detail": "selected local files are still being prepared"},
            {"code": "global_access_disabled"}
        ],
        "pagination": {"page_index": 0, "total_pages": 1}
    }));

    golden(
        &map,
        &[
            "map 3f9a1c2e",
            "languages:",
            "\tmarkdown · 1 file · 1 symbol",
            "\ttypescript:tsx · 2 files · 0 symbols",
            "modules:",
            "\tcrates · 9 files · 80 symbols",
            "\t\trift-mcp · 4 files · 40 symbols",
            "\t\t\tsrc · 3 files · 30 symbols",
            "\t\t\t\toutput · 1 file · 10 symbols",
            "\t\t\ttests · 1 file · 10 symbols",
            "\t\trift-server · 5 files · 40 symbols",
            "\tcrates-extra · 1 file · 1 symbol",
            "\t\telsewhere · 1 file · 1 symbol",
            "\t\tsub · 1 file · 1 symbol",
            "\tdocs · 0 files · 0 symbols",
            "hubs:",
            "\trift://symbol/rust/crates/rift-core/src/lib.rs/Revision · struct · 412 references",
            "\trift://symbol/rust/crates/rift-core/src/lib.rs/Once · function_item · 1 reference",
            "entry points:",
            "\trift://symbol/rust/crates/rift/src/main.rs/main",
            "\trift://symbol/rust/crates/rift-cli/src/main.rs/cli",
            "docs:",
            "\tREADME.md",
            "\tdocs/guide.md",
            "module relationships:",
            "\tcrates/rift-mcp → crates/rift-server · 312 references",
            "\tcrates/rift-server → crates/rift-mcp · 1 reference",
            "packages:",
            "\tserde ^1.0 (cargo)",
            "\ttokio@1.53.1 (cargo)",
            "\tlocal * (cargo, path)",
            "\tprivate@2.0.0 (npm, private_registry)",
            "2 warnings",
            "\tlocal_index_preparing · prepared 0 · total 2: selected local files are still being prepared",
            "\tglobal_access_disabled",
        ],
    );
}

#[test]
fn an_empty_map_is_its_header_alone() {
    let map =
        map_of(json!({"revision": "3f9a1c2e", "pagination": {"page_index": 0, "total_pages": 1}}));
    golden(&map, &["map 3f9a1c2e"]);
}

#[test]
fn a_map_with_warnings_only_is_its_header_and_the_warnings_section() {
    let map = map_of(json!({
        "revision": "3f9a1c2e",
        "warnings": [{"code": "global_access_disabled"}],
        "pagination": {"page_index": 0, "total_pages": 1}
    }));
    golden(
        &map,
        &["map 3f9a1c2e", "1 warning", "\tglobal_access_disabled"],
    );
}

#[test]
fn a_map_never_names_its_page() {
    let map =
        map_of(json!({"revision": "3f9a1c2e", "pagination": {"page_index": 4, "total_pages": 9}}));
    golden(&map, &["map 3f9a1c2e"]);
}

#[test]
fn a_map_cut_to_the_byte_limit_yields_no_text() {
    let mut out = TextWriter::new(4);
    let refused = authored_map().render(&mut out);
    assert_eq!(
        refused,
        Err(TextError::Overflow(OutputOverflow { limit: 4 }))
    );
    assert!(out.finish().is_err(), "no partial text is returned");
}

#[test]
fn a_package_names_its_availability_only_when_it_is_not_canonical() {
    assert_eq!(
        package_text(("cargo", "serde"), ("1.0.0", ""), CANONICAL_AVAILABILITY),
        "serde@1.0.0 (cargo)"
    );
    assert_eq!(
        package_text(("cargo", "serde"), ("", "^1.0"), "git"),
        "serde ^1.0 (cargo, git)"
    );
    assert_eq!(
        package_text(("cargo", "serde"), ("", ""), "url"),
        "serde (cargo, url)"
    );
    assert_eq!(
        wire_name(&PackageAvailability::Canonical).expect("a unit variant"),
        CANONICAL_AVAILABILITY
    );
}

/// The effective language entry of the protocol example.
fn tsx() -> Value {
    json!({
        "language": "typescript:tsx", "enabled": true,
        "include": ["src/**/*.tsx"], "exclude": ["src/generated/**"],
        "execution": false, "syntax": true,
        "lsp": {"process": "typescript:tsx", "state": "ready"}
    })
}

#[test]
fn one_workspace_page_renders_languages_and_source() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e",
        "languages": [tsx()],
        "source": [{"path": "src/view.tsx", "digest": "8a4d20bc", "language": "typescript:tsx"}],
        "pagination": {"page_index": 0, "total_pages": 1}
    }));
    golden(
        &page,
        &[
            "workspace 3f9a1c2e",
            "languages:",
            "\ttypescript:tsx · syntax · lsp typescript:tsx ready",
            "\t\tinclude src/**/*.tsx",
            "\t\texclude src/generated/**",
            "source:",
            "\tsrc/view.tsx · typescript:tsx · 8a4d20bc",
        ],
    );
}

#[test]
fn several_workspace_pages_name_the_page_in_the_header() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e",
        "languages": [{"language": "python", "enabled": false, "execution": false, "syntax": false}],
        "source": [{"path": "src/view.tsx", "digest": "8a4d20bc"}],
        "pagination": {"page_index": 0, "total_pages": 3}
    }));
    golden(
        &page,
        &[
            "workspace 3f9a1c2e · page 1/3",
            "languages:",
            "\tpython · disabled",
            "source:",
            "\tsrc/view.tsx · 8a4d20bc",
        ],
    );
}

#[test]
fn a_page_past_the_end_names_itself_and_holds_no_source() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e",
        "languages": [], "source": [],
        "pagination": {"page_index": 4, "total_pages": 3}
    }));
    golden(&page, &["workspace 3f9a1c2e · page 5/3"]);
}

#[test]
fn an_empty_workspace_with_one_page_is_its_header_alone() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e",
        "languages": [], "source": [],
        "pagination": {"page_index": 0, "total_pages": 0}
    }));
    golden(&page, &["workspace 3f9a1c2e"]);
}

#[test]
fn a_language_writes_each_capability_and_every_lsp_state() {
    let languages: Vec<Value> = ["stopped", "starting", "analyzing", "ready", "failed"]
        .iter()
        .map(|state| {
            json!({
                "language": format!("lang-{state}"), "enabled": true,
                "execution": true, "syntax": true,
                "lsp": {"process": "server", "state": state}
            })
        })
        .collect();
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e",
        "languages": languages, "source": [],
        "pagination": {"page_index": 0, "total_pages": 1}
    }));
    golden(
        &page,
        &[
            "workspace 3f9a1c2e",
            "languages:",
            "\tlang-stopped · syntax · execution · lsp server stopped",
            "\tlang-starting · syntax · execution · lsp server starting",
            "\tlang-analyzing · syntax · execution · lsp server analyzing",
            "\tlang-ready · syntax · execution · lsp server ready",
            "\tlang-failed · syntax · execution · lsp server failed",
        ],
    );
}

#[test]
fn patterns_join_by_comma_and_an_empty_list_writes_no_line() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e",
        "languages": [
            {"language": "rust", "enabled": true, "include": ["a/**", "b/**"],
             "execution": false, "syntax": true},
            {"language": "go", "enabled": true, "exclude": ["vendor/**"],
             "execution": false, "syntax": false}
        ],
        "source": [],
        "pagination": {"page_index": 0, "total_pages": 1}
    }));
    golden(
        &page,
        &[
            "workspace 3f9a1c2e",
            "languages:",
            "\trust · syntax",
            "\t\tinclude a/**, b/**",
            "\tgo",
            "\t\texclude vendor/**",
        ],
    );
}

#[test]
fn a_workspace_page_writes_its_warnings_after_the_last_section() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e",
        "languages": [],
        "source": [
            {"path": "a.rs", "digest": "00000001", "language": "rust"},
            {"path": "notes.txt", "digest": "00000002"}
        ],
        "warnings": [{"code": "local_index_preparing", "prepared": 1,
                      "detail": "selected local files are still being prepared"}],
        "pagination": {"page_index": 1, "total_pages": 2}
    }));
    golden(
        &page,
        &[
            "workspace 3f9a1c2e · page 2/2",
            "source:",
            "\ta.rs · rust · 00000001",
            "\tnotes.txt · 00000002",
            "1 warning",
            "\tlocal_index_preparing · prepared 1: selected local files are still being prepared",
        ],
    );
}

/// One record with the given labels and fields.
fn line<'a>(
    recorded_at_ms: i64,
    (level, component, operation): (&'a str, &'a str, &'a str),
    message: &'a str,
    fields: LogFields<'a>,
) -> LogLine<'a> {
    LogLine {
        identity: 1,
        recorded_at_ms,
        level,
        target: "rift_mcp::server",
        component,
        operation,
        message,
        fields,
    }
}

fn object(text: &str) -> LogFields<'_> {
    LogFields::parse(text)
}

fn page(records: Vec<LogLine<'_>>) -> LogsPage<'_> {
    LogsPage {
        records,
        unavailable: None,
    }
}

#[test]
fn no_line_of_a_resource_text_starts_with_a_space() {
    let workspace = page_of(json!({
        "configuration_revision": "3f9a1c2e",
        "languages": [tsx()],
        "source": [{"path": "src/view.tsx", "digest": "8a4d20bc", "language": "typescript:tsx"}],
        "pagination": {"page_index": 0, "total_pages": 1}
    }));
    let logs = page(vec![line(
        1_791_110_527_120,
        ("warn", "index", "index.reconcile"),
        "the capture disagreed",
        object(r#"{"epoch":"4"}"#),
    )]);
    for text in [
        rendered(&authored_map()),
        rendered(&workspace),
        rendered(&logs),
    ] {
        for line in text.lines() {
            assert!(
                !line.starts_with(' '),
                "a line starts with a space: {line:?} in {text}"
            );
        }
    }
}

#[test]
fn logs_render_one_line_per_record_with_object_fields() {
    let logs = page(vec![
        line(
            1_791_110_527_120,
            ("warn", "index", "index.reconcile"),
            "the capture disagreed",
            object(r#"{"epoch":"4"}"#),
        ),
        line(
            1_791_110_526_004,
            ("info", "search", "search.query"),
            "",
            object(r#"{"elapsed_ms":12}"#),
        ),
        line(
            1_791_110_526_000,
            ("debug", "engine", "engine.start"),
            "ready",
            object("{}"),
        ),
    ]);
    golden(
        &logs,
        &[
            "3 records",
            "\t2026-10-04 10:42:07.120 warn index index.reconcile: the capture disagreed · epoch 4",
            "\t2026-10-04 10:42:06.004 info search search.query · elapsed_ms 12",
            "\t2026-10-04 10:42:06.000 debug engine engine.start: ready",
        ],
    );
}

#[test]
fn logs_write_non_object_fields_as_text_and_values_compactly() {
    let logs = page(vec![
        line(0, ("error", "mcp", "mcp.read"), "plain", object("not json")),
        line(0, ("error", "mcp", "mcp.read"), "list", object("[1,2]")),
        line(0, ("error", "mcp", "mcp.read"), "none", object("")),
        line(
            0,
            ("error", "mcp", "mcp.read"),
            "mixed",
            object(r#"{"a":true,"b":null,"c":[1,"x"],"d":{"e":1},"f":"","g":"two words"}"#),
        ),
    ]);
    golden(
        &logs,
        &[
            "4 records",
            "\t1970-01-01 00:00:00.000 error mcp mcp.read: plain · not json",
            "\t1970-01-01 00:00:00.000 error mcp mcp.read: list · [1,2]",
            "\t1970-01-01 00:00:00.000 error mcp mcp.read: none",
            "\t1970-01-01 00:00:00.000 error mcp mcp.read: mixed · a true · b null · c [1,\"x\"] · d {\"e\":1} · f \"\" · g two words",
        ],
    );
}

#[test]
fn logs_mark_a_missing_component_and_operation_and_hide_control_characters() {
    let logs = page(vec![line(
        0,
        ("info", "", ""),
        "two\nlines\tand a tab",
        object(r#"{"path":"a\nb"}"#),
    )]);
    golden(
        &logs,
        &[
            "1 record",
            "\t1970-01-01 00:00:00.000 info - -: two\\nlines\\tand a tab · path a\\nb",
        ],
    );
}

#[test]
fn logs_quote_a_message_or_value_that_holds_a_delimiter() {
    let logs = page(vec![
        line(
            0,
            ("warn", "mcp", "mcp.read"),
            "read failed: a \"b\" · c\\d",
            object(r#"{"note":"k: v","path":"a · b","plain":"a:b"}"#),
        ),
        line(0, ("warn", "mcp", "mcp.read"), "plain", object("x · y")),
    ]);
    golden(
        &logs,
        &[
            "2 records",
            "\t1970-01-01 00:00:00.000 warn mcp mcp.read: \"read failed: a \\\"b\\\" · c\\\\d\" · note \"k: v\" · path \"a · b\" · plain a:b",
            "\t1970-01-01 00:00:00.000 warn mcp mcp.read: plain · \"x · y\"",
        ],
    );
    let unavailable = LogsPage {
        records: Vec::new(),
        unavailable: Some("open failed: denied"),
    };
    golden(
        &unavailable,
        &[
            "0 records",
            "1 warning",
            "\tunavailable: \"open failed: denied\"",
        ],
    );
}

#[test]
fn an_unavailable_store_is_zero_records_and_one_warning_with_its_reason() {
    let logs = LogsPage {
        records: Vec::new(),
        unavailable: Some("the workspace log store could not be opened"),
    };
    golden(
        &logs,
        &[
            "0 records",
            "1 warning",
            "\tunavailable: the workspace log store could not be opened",
        ],
    );
}

#[test]
fn zero_records_are_the_header_alone() {
    golden(&page(Vec::new()), &["0 records"]);
}

#[test]
fn log_fields_keep_the_object_and_the_text_the_wire_carries() {
    let parsed = LogFields::parse(r#"{"epoch":"4"}"#);
    let mut expected = Map::new();
    expected.insert("epoch".to_owned(), json!("4"));
    assert_eq!(parsed, LogFields::Object(expected));
    assert_eq!(parsed.to_json(), json!({"epoch": "4"}));
    assert_eq!(LogFields::parse("[1]").to_json(), json!("[1]"));
    assert_eq!(LogFields::parse("").to_json(), json!(""));
}

#[test]
fn time_is_utc_with_milliseconds() {
    assert_eq!(utc_time(0), "1970-01-01 00:00:00.000");
    assert_eq!(utc_time(999), "1970-01-01 00:00:00.999");
    assert_eq!(utc_time(1_709_210_096_789), "2024-02-29 12:34:56.789");
    assert_eq!(utc_time(1_704_067_199_999), "2023-12-31 23:59:59.999");
    assert_eq!(utc_time(1_704_067_200_000), "2024-01-01 00:00:00.000");
    assert_eq!(utc_time(951_782_400_000), "2000-02-29 00:00:00.000");
    assert_eq!(utc_time(4_107_542_399_000), "2100-02-28 23:59:59.000");
    assert_eq!(utc_time(4_107_542_400_000), "2100-03-01 00:00:00.000");
    assert_eq!(utc_time(253_402_207_200_000), "9999-12-30 22:00:00.000");
    assert_eq!(utc_time(-1), "1969-12-31 23:59:59.999");
}

#[test]
fn a_time_outside_the_range_of_a_jiff_timestamp_is_the_count() {
    assert_eq!(utc_time(253_402_207_200_001), "253402207200001");
    assert_eq!(utc_time(253_402_300_799_999), "253402300799999");
    assert_eq!(utc_time(-377_705_023_201_001), "-377705023201001");
    assert_eq!(utc_time(i64::MIN), i64::MIN.to_string());
    assert_eq!(utc_time(i64::MAX), i64::MAX.to_string());
}

#[test]
fn a_warning_names_a_package_availability_that_is_not_canonical() {
    let map = map_of(json!({
        "revision": "3f9a1c2e",
        "warnings": [{
            "code": "package_requirement_absent",
            "entry": {"manager": "cargo", "name": "local", "requirement": "*", "availability": "git"}
        }],
        "pagination": {"page_index": 0, "total_pages": 1}
    }));
    golden(
        &map,
        &[
            "map 3f9a1c2e",
            "1 warning",
            "\tpackage_requirement_absent · entry local * (cargo, git)",
        ],
    );
}
