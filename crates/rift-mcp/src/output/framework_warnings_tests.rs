use std::sync::Arc;

use rift_protocol::symbol_read::GetSymbolResult;
use rmcp::handler::server::tool::IntoCallToolResult as _;
use rmcp::model::{CallToolResponse, CallToolResult};
use serde_json::json;

use super::{FRAMEWORK_WARNINGS_MAX, FrameworkWarnings};
use crate::output::Json;

fn missing(owner: &str, detail: &str, file: &str) -> CallToolResult {
    let answer: GetSymbolResult = serde_json::from_value(json!({
        "outcome":"missing",
        "symbol_not_found":{
            "id":format!("rift://symbol/{owner}/rust/app/missing"),
            "alternatives":[{
                "id":format!("rift://symbol/{owner}/rust/app/beacon"),
                "name":"beacon", "match_context":[]
            }]
        },
        "view":{"id":"a".repeat(64),"expires_at":"2030-01-01T00:00:00Z"},
        "warnings":[
            {"code":"framework_context_unresolved", "unit":format!("rift://file/{file}"),
             "framework":"tailwind", "detail":detail},
            {"code":"global_access_disabled"}
        ]
    }))
    .expect("typed missing symbol");
    completed(answer)
}

fn completed(answer: GetSymbolResult) -> CallToolResult {
    let Ok(CallToolResponse::Complete(result)) = Json(answer).into_call_tool_result() else {
        panic!("missing symbol completes the tool call");
    };
    result
}

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().expect("text answer").text
}

fn has_framework(result: &CallToolResult) -> bool {
    text(result).contains("framework_context_unresolved")
}

#[test]
fn repeated_symbol_warning_text_keeps_captured_facts_errors_and_alternatives() {
    let warnings = FrameworkWarnings::default();
    let answer = missing("local", "missing version", "a.ts");
    assert_eq!(answer.is_error, Some(true));
    let first = warnings.select_result(answer.clone(), "local");
    let repeated = warnings.select_result(answer.clone(), "local");
    assert!(has_framework(&first));
    assert!(!has_framework(&repeated));
    for result in [&first, &repeated] {
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content, answer.structured_content);
        assert!(text(result).contains("error: symbol_not_found"));
        assert!(text(result).contains("closest matches:"));
        assert!(text(result).contains("rift://symbol/local/rust/app/beacon"));
        assert!(text(result).contains("global_access_disabled"));
    }
    assert!(has_framework(
        &FrameworkWarnings::default().select_result(answer, "local")
    ));
}

#[test]
fn changed_files_and_details_reappear_while_interleaved_profiles_stay_independent() {
    let warnings = FrameworkWarnings::default();
    let first = missing("local", "missing version", "a.ts");
    let profile = missing("local@docs", "missing version", "a.ts");
    assert!(has_framework(
        &warnings.select_result(first.clone(), "local")
    ));
    assert!(has_framework(
        &warnings.select_result(profile.clone(), "local")
    ));
    assert!(!has_framework(
        &warnings.select_result(first.clone(), "local")
    ));
    assert!(!has_framework(&warnings.select_result(profile, "local")));
    for answer in [
        missing("local", "missing version", "b.ts"),
        first.clone(),
        missing("local", "missing configuration", "a.ts"),
        first,
    ] {
        assert!(has_framework(&warnings.select_result(answer, "local")));
    }
}

#[test]
fn concurrent_proxy_clones_show_one_unchanged_group() {
    let warnings = Arc::new(FrameworkWarnings::default());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let warnings = Arc::clone(&warnings);
            std::thread::spawn(move || {
                warnings.select_result(missing("local", "missing version", "a.ts"), "local")
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("warning selection finishes"))
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|result| has_framework(result))
            .count(),
        1
    );
    assert!(results.iter().all(|result| result.is_error == Some(true)));
    assert!(
        results
            .windows(2)
            .all(|pair| { pair[0].structured_content == pair[1].structured_content })
    );
}

#[test]
fn connection_warning_bound_evicts_oldest_owner_and_keeps_other_owners() {
    let warnings = FrameworkWarnings::default();
    for index in 0..=FRAMEWORK_WARNINGS_MAX {
        let answer = missing(&format!("local@profile{index}"), "missing version", "a.ts");
        assert!(has_framework(&warnings.select_result(answer, "local")));
    }
    assert_eq!(
        warnings.shown.lock().expect("warning state").len(),
        FRAMEWORK_WARNINGS_MAX
    );
    assert!(!has_framework(&warnings.select_result(
        missing("local@profile1", "missing version", "a.ts"),
        "local"
    )));
    assert!(has_framework(&warnings.select_result(
        missing("local@profile0", "missing version", "a.ts"),
        "local"
    )));
}

#[test]
fn resolved_framework_context_then_reappearing_warning_is_shown_again() {
    let warnings = FrameworkWarnings::default();
    let answer = missing("local", "missing version", "a.ts");
    assert!(has_framework(
        &warnings.select_result(answer.clone(), "local")
    ));
    let profile = missing("local@docs", "missing version", "a.ts");
    assert!(has_framework(
        &warnings.select_result(profile.clone(), "local")
    ));
    let mut resolved = answer.clone();
    resolved.structured_content.as_mut().expect("typed answer")["warnings"] = json!([]);
    let resolved = completed(
        serde_json::from_value(resolved.structured_content.expect("typed answer"))
            .expect("resolved symbol answer"),
    );
    assert!(!has_framework(&warnings.select_result(resolved, "local")));
    assert!(!has_framework(&warnings.select_result(profile, "local")));
    assert!(has_framework(&warnings.select_result(answer, "local")));
}

#[test]
fn package_symbol_warnings_keep_originating_project_context() {
    let warnings = FrameworkWarnings::default();
    let answer = missing("cargo/crates.io/beacon@1.0.0", "missing version", "a.rs");
    assert!(has_framework(
        &warnings.select_result(answer.clone(), "local")
    ));
    assert!(has_framework(
        &warnings.select_result(answer.clone(), "docs")
    ));
    assert!(!has_framework(
        &warnings.select_result(answer.clone(), "local")
    ));
    assert!(!has_framework(&warnings.select_result(answer, "docs")));
}
