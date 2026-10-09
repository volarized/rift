//! Goldens of workspace resource text.

use rift_protocol::dependencies::PackageAvailability;
use rift_protocol::workspace::WorkspaceResourcePage;
use serde_json::{Value, json};

use super::facts::{CANONICAL_AVAILABILITY, package_text, wire_name};
use super::{Render, text_of};

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

fn page_of(value: Value) -> WorkspaceResourcePage {
    serde_json::from_value(value).expect("the workspace fixture deserializes")
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
