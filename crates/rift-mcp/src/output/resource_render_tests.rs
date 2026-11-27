//! Goldens of workspace resource text.

use rift_protocol::workspace::WorkspaceResourcePage;
use serde_json::{Value, json};

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
        "configuration_revision": "3f9a1c2e", "revision": "rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "languages": [tsx()],
        "source": [{"path": "src/view.tsx", "digest": "8a4d20bc", "language": "typescript:tsx"}],
        "pagination": {"page": 1, "pages": 1, "max_results": 10, "total": 10}
    }));
    golden(
        &page,
        &[
            "workspace",
            "languages:",
            "\ttypescript:tsx | syntax | lsp typescript:tsx ready",
            "\t\tinclude src/**/*.tsx",
            "\t\texclude src/generated/**",
            "source:",
            "\tsrc/view.tsx | typescript:tsx",
        ],
    );
}

#[test]
fn several_workspace_pages_name_the_page_in_the_header() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e", "revision": "rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "languages": [{"language": "python", "enabled": false, "execution": false, "syntax": false}],
        "source": [{"path": "src/view.tsx", "digest": "8a4d20bc"}],
        "pagination": {"page": 1, "pages": 3, "max_results": 10, "total": 30}
    }));
    golden(
        &page,
        &[
            "workspace | page 1/3, total 30 results",
            "languages:",
            "\tpython | disabled",
            "source:",
            "\tsrc/view.tsx",
            "next | rift://workspace?page=2&max_results=10&revision=rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ],
    );
}

#[test]
fn a_page_past_the_end_names_itself_and_holds_no_source() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e", "revision": "rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "languages": [], "source": [],
        "pagination": {"page": 4, "pages": 3, "max_results": 10, "total": 30}
    }));
    golden(
        &page,
        &[
            "workspace | page 4/3, total 30 results",
            "No source files on this page.",
        ],
    );
}

#[test]
fn an_empty_workspace_with_one_page_is_its_header_alone() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e", "revision": "rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "languages": [], "source": [],
        "pagination": {"page": 1, "pages": 0, "max_results": 10, "total": 0}
    }));
    golden(&page, &["workspace", "No source files on this page."]);
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
        "configuration_revision": "3f9a1c2e", "revision": "rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "languages": languages, "source": [],
        "pagination": {"page": 1, "pages": 1, "max_results": 10, "total": 10}
    }));
    golden(
        &page,
        &[
            "workspace",
            "languages:",
            "\tlang-stopped | syntax | execution | lsp server stopped",
            "\tlang-starting | syntax | execution | lsp server starting",
            "\tlang-analyzing | syntax | execution | lsp server analyzing",
            "\tlang-ready | syntax | execution | lsp server ready",
            "\tlang-failed | syntax | execution | lsp server failed",
        ],
    );
}

#[test]
fn patterns_join_by_comma_and_an_empty_list_writes_no_line() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e", "revision": "rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "languages": [
            {"language": "rust", "enabled": true, "include": ["a/**", "b/**"],
             "execution": false, "syntax": true},
            {"language": "go", "enabled": true, "exclude": ["vendor/**"],
             "execution": false, "syntax": false}
        ],
        "source": [],
        "pagination": {"page": 1, "pages": 1, "max_results": 10, "total": 10}
    }));
    golden(
        &page,
        &[
            "workspace",
            "languages:",
            "\trust | syntax",
            "\t\tinclude a/**, b/**",
            "\tgo",
            "\t\texclude vendor/**",
        ],
    );
}

#[test]
fn warnings_leave_workspace_resource_text_unchanged() {
    let page = page_of(json!({
        "configuration_revision": "3f9a1c2e", "revision": "rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "languages": [],
        "source": [
            {"path": "a.rs", "digest": "00000001", "language": "rust"},
            {"path": "notes.txt", "digest": "00000002"}
        ],
        "warnings": [{"code": "local_index_preparing", "prepared": 1,
                      "detail": "selected local files are still being prepared"}],
        "pagination": {"page": 1, "pages": 2, "max_results": 10, "total": 20}
    }));
    golden(
        &page,
        &[
            "workspace | page 1/2, total 20 results",
            "source:",
            "\ta.rs | rust",
            "\tnotes.txt",
            "next | rift://workspace?page=2&max_results=10&revision=rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ],
    );
}
