use super::{valid_result, validate_request};
use rift_error::{ErrorValue, RiftError};
use rift_protocol::{
    read::GetSymbolInclude,
    symbol_read::{GetSymbolParams, GetSymbolResult},
};
use serde_json::{Value, json};

const ID: &str = "rift://symbol/cargo/crates.io/tokio/rust/tokio/Beacon";
const RELEASE: &str = "rift://revision/cargo/crates.io/tokio@1.50.0";
const OTHER_ID: &str = "rift://symbol/cargo/crates.io/tokio/rust/tokio/Other";

fn error_field<'error>(error: &'error RiftError, key: &str) -> Option<&'error str> {
    error.fields().iter().find_map(|field| {
        if field.key() != key {
            return None;
        }
        match field.value() {
            ErrorValue::Display(value) => Some(value.as_str()),
            _ => None,
        }
    })
}

fn assert_request_field(error: &RiftError, field: &str) {
    assert_eq!(error.slug().as_str(), "rift.client.request_invalid");
    assert_eq!(error_field(error, "field"), Some(field));
}

fn revision(value: &str) -> rift_protocol::revision::Revision {
    rift_protocol::revision::Revision::parse(value).expect("revision fixture")
}

fn request() -> GetSymbolParams {
    serde_json::from_value(json!({"id": ID})).expect("request")
}

fn view() -> Value {
    json!({"id": "a".repeat(64), "expires_at": "2026-10-10T01:00:00Z"})
}

fn found(declarations: Vec<Value>) -> GetSymbolResult {
    serde_json::from_value(json!({
        "outcome": "found",
        "symbol": {"id": ID, "language": "rust", "name": "Beacon", "kind": "struct", "origin": {
            "location": "dependency", "source_kind": "authored", "package": {
                "manager": "cargo", "registry": "crates.io", "name": "tokio", "version": "1.50.0"
            }
        }},
        "view": view(),
        "revision": RELEASE,
        "declarations": Value::Array(declarations),
    }))
    .expect("result")
}

fn declaration(end: usize) -> Value {
    json!({
        "origin": {"location": "dependency", "source_kind": "authored", "package": {
            "manager": "cargo", "registry": "crates.io", "name": "tokio", "version": "1.50.0"
        }},
        "unit": "rift://source/cargo/crates.io/tokio/src/lib.rs",
        "range": {"start": 0, "end": end},
        "line": 1,
        "signature_indices": [],
        "type_indices": [],
        "documentation_indices": [],
    })
}

#[test]
fn source_less_found_and_typed_absence_remain_distinct() {
    let request = request();
    assert!(valid_result(&request, &found(Vec::new()), 100));
    let mut intrinsic = serde_json::to_value(found(Vec::new())).expect("source-less symbol");
    intrinsic["symbol"]["origin"] = json!({"source_kind": "synthetic"});
    let intrinsic: GetSymbolResult =
        serde_json::from_value(intrinsic).expect("synthetic source-less symbol");
    assert!(valid_result(&request, &intrinsic, 100));
    let missing = serde_json::from_value(json!({
        "outcome": "missing", "view": view(), "revision": RELEASE,
        "symbol_not_found": {"id": ID, "alternatives": [{
            "id": OTHER_ID, "name": "Beacon", "match_context": ["version"]
        }]}
    }))
    .expect("missing");
    assert!(valid_result(&request, &missing, 100));
    let unavailable = serde_json::from_value(json!({
        "outcome": "unavailable", "id": ID, "reason": "exact_release"
    }))
    .expect("unavailable");
    assert!(valid_result(&request, &unavailable, 100));
}

#[test]
fn exact_answer_rejects_a_different_release_or_captured_view() {
    let mut request = request();
    let mut result = found(Vec::new());
    if let GetSymbolResult::Found { symbol, .. } = &mut result {
        symbol.id = Some(rift_protocol::read::SymbolId::parse(OTHER_ID).expect("identity"));
    }
    assert!(!valid_result(&request, &result, 100));
    request.revision = Some(revision("rift://revision/cargo/crates.io/tokio@1.49.0"));
    assert!(
        !valid_result(&request, &found(Vec::new()), 100),
        "an answer reads the release the request named"
    );
    request.revision = Some(revision(RELEASE));
    assert!(valid_result(&request, &found(Vec::new()), 100));
    let mut stale = serde_json::to_value(found(Vec::new())).expect("found");
    stale["symbol"]["origin"]["package"]["version"] = json!("1.49.0");
    let stale: GetSymbolResult = serde_json::from_value(stale).expect("stale origin");
    assert!(
        !valid_result(&request, &stale, 100),
        "the symbol's origin names the answered release"
    );
    request.view =
        Some(rift_protocol::symbol_read::CapturedViewId::parse(&"b".repeat(64)).expect("view"));
    assert!(!valid_result(&request, &found(Vec::new()), 100));
}

#[test]
fn exact_answer_enforces_projection_and_declaration_bounds() {
    let mut request = request();
    let mut invalid_binding = declaration(18);
    invalid_binding["signature_indices"] = json!([0]);
    assert!(!valid_result(&request, &found(vec![invalid_binding]), 100));
    let result = found(vec![declaration(18)]);
    assert!(valid_result(&request, &result, 100));
    let mut foreign = declaration(18);
    foreign["unit"] = json!("rift://source/cargo/crates.io/other/src/lib.rs");
    assert!(!valid_result(&request, &found(vec![foreign]), 100));
    request.declaration_limit = 1;
    let result = found(vec![declaration(18), declaration(14)]);
    assert!(!valid_result(&request, &result, 100));
}

#[test]
fn request_rejects_local_owners_revision_and_unbound_continuation() {
    let local: GetSymbolParams = serde_json::from_value(json!({
        "id": "rift://symbol/local/rust/Beacon"
    }))
    .expect("local request");
    assert_request_field(&validate_request(&local).expect_err("local owner"), "id");
    let mut request = request();
    request.declaration_cursor = Some("next".to_owned());
    assert_request_field(
        &validate_request(&request).expect_err("unbound cursor"),
        "declaration_cursor",
    );
    request.declaration_cursor = None;
    request.declaration_limit = 0;
    assert_request_field(
        &validate_request(&request).expect_err("declaration bound"),
        "declaration_limit",
    );
    request.declaration_limit = 5;
    request.revision = Some(revision("rift://revision/git/main"));
    assert_request_field(
        &validate_request(&request).expect_err("revision"),
        "revision",
    );
    request.revision = Some(revision("rift://revision/cargo/crates.io/serde@1.0.228"));
    assert_request_field(
        &validate_request(&request).expect_err("release of another package"),
        "revision",
    );
    for release in [
        "rift://revision/cargo/crates.io/tokio@1.49.0",
        "rift://revision/cargo/crates.io/tokio@1.50.0",
    ] {
        request.revision = Some(revision(release));
        assert!(validate_request(&request).is_ok(), "{release}");
    }
}

#[test]
fn exact_response_refuses_legacy_hit_pages() {
    let meta = crate::ResponseMeta {
        status: 200,
        content_type: Some("application/json".to_owned()),
        etag: None,
        cache_control: None,
        www_authenticate: None,
        retry_after: None,
        rate_limit_limit: None,
        rate_limit_remaining: None,
        rate_limit_reset: None,
    };
    let response = crate::RawResponse {
        status: reqwest::StatusCode::OK,
        body: serde_json::to_vec(&json!({"items": [], "warnings": []})).expect("body"),
        meta,
    };
    let error = crate::response::exact_symbol(response)
        .err()
        .expect("legacy page");
    assert_eq!(error.slug().as_str(), "rift.client.response_invalid");
    assert_eq!(error_field(&error, "status"), Some("200"));
    assert_eq!(error_field(&error, "field"), Some("body"));
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn exact_response_requires_status_to_match_outcome() {
    let outcomes = [
        (200, serde_json::to_value(found(Vec::new())).expect("found")),
        (
            404,
            json!({"outcome":"missing","symbol_not_found":{"id":ID,"alternatives":[]},"view":view(),"revision":RELEASE}),
        ),
        (
            503,
            json!({"outcome":"unavailable","id":ID,"reason":"exact_release"}),
        ),
    ];
    for (expected, value) in outcomes {
        for status in [200, 404, 503] {
            let status_code = reqwest::StatusCode::from_u16(status).expect("status");
            let headers = [(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            )]
            .into_iter()
            .collect();
            let response = crate::RawResponse {
                status: status_code,
                body: serde_json::to_vec(&value).expect("body"),
                meta: crate::ResponseMeta::from_headers(status_code, &headers).expect("headers"),
            };
            let result = crate::response::exact_symbol(response);
            if status == expected {
                assert!(result.is_ok(), "accepted status {status}");
            } else {
                let error = result.err().expect("outcome status mismatch");
                assert_eq!(error.slug().as_str(), "rift.client.response_invalid");
                assert_eq!(
                    error_field(&error, "status"),
                    Some(status.to_string().as_str())
                );
                assert_eq!(error_field(&error, "field"), Some("status"));
            }
        }
    }
}

#[test]
fn exact_response_keeps_service_problem_separate_from_unavailable() {
    let status = reqwest::StatusCode::SERVICE_UNAVAILABLE;
    for media in ["application/problem+json", "text/plain"] {
        let headers = [(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static(media),
        )]
        .into_iter()
        .collect();
        let response = crate::RawResponse {
            status,
            body: serde_json::to_vec(
                &json!({"type":"about:blank","title":"Service Unavailable","status":503,"detail":"fixture problem"}),
            )
            .expect("problem"),
            meta: crate::ResponseMeta::from_headers(status, &headers).expect("headers"),
        };
        let error = crate::response::exact_symbol(response)
            .err()
            .expect("remote failure");
        assert_eq!(error.slug().as_str(), "rift.client.remote_unavailable");
        assert_eq!(error_field(&error, "status"), Some("503"));
        let problem = error_field(&error, "problem");
        assert_eq!(problem.is_some(), media == "application/problem+json");
        if let Some(problem) = problem {
            let problem: Value = serde_json::from_str(problem).expect("problem evidence");
            assert_eq!(problem["status"], 503);
            assert_eq!(problem["detail"], "fixture problem");
        }
    }
}

fn documented_result() -> Value {
    let content = json!({"source": {"kind": "package", "unit": "rift://source/cargo/crates.io/tokio/README.md"}});
    let mut result = serde_json::to_value(found(vec![declaration(18)])).expect("found");
    result["documentation"] = json!({
        "documentation_revision": "0123abcd", "truncated": false,
        "references": [{
            "reference": {"identity": "5".repeat(64), "block": "1".repeat(64),
                "target": ID, "range": {"start": 1, "end": 7},
                "authored": "Beacon", "evidence": "unique_name"},
            "documentation": {
                "documentation_revision": "0123abcd",
                "block": {"identity": "1".repeat(64), "source": content,
                    "content_digest": "2".repeat(64), "range": {"start": 0, "end": 40},
                    "line": 1, "kind": "prose"},
                "source": {"identity": content, "revision": "3".repeat(64), "content_digest": "4".repeat(64),
                    "origin": {"location": "dependency", "source_kind": "authored", "package": {
                        "manager": "cargo", "registry": "crates.io", "name": "tokio", "version": "1.50.0"}},
                    "format": "markdown", "media_type": "text/markdown", "selection": "package_archive", "byte_length": 40}
            }, "excerpt": "guide"
        }]
    });
    result
}

#[test]
fn canonical_documentation_keeps_original_associations_and_refuses_tampered_facts() {
    let mut request = request();
    request.include.push(GetSymbolInclude::Documentation);
    let original = documented_result();
    let result: GetSymbolResult =
        serde_json::from_value(original.clone()).expect("documented result");
    assert!(valid_result(&request, &result, 100));
    let mut source_less = original.clone();
    source_less["declarations"] = json!([]);
    let source_less = serde_json::from_value(source_less).expect("source-less documented symbol");
    assert!(valid_result(&request, &source_less, 5));
    assert!(!valid_result(&request, &source_less, 4));
    let mut intrinsic = original.clone();
    intrinsic["declarations"] = json!([]);
    intrinsic["symbol"]["origin"] = json!({"source_kind": "synthetic"});
    let intrinsic = serde_json::from_value(intrinsic).expect("synthetic documented symbol");
    assert!(
        !valid_result(&request, &intrinsic, 100),
        "package documentation needs the answered release in the symbol's origin"
    );
    for (pointer, changed) in [
        (
            "/documentation/references/0/reference/target",
            json!(OTHER_ID),
        ),
        (
            "/documentation/references/0/reference/block",
            json!("6".repeat(64)),
        ),
        ("/documentation/references/0/reference/range/end", json!(41)),
        (
            "/documentation/references/0/documentation/documentation_revision",
            json!("abcdef01"),
        ),
        (
            "/documentation/references/0/documentation/source/origin/package/version",
            json!("1.49.0"),
        ),
        (
            "/documentation/references/0/documentation/source/identity/source/unit",
            json!("rift://source/cargo/crates.io/other/README.md"),
        ),
        (
            "/documentation/references/0/documentation/source/revision",
            json!("invalid"),
        ),
        (
            "/documentation/references/0/documentation/source/byte_length",
            json!(39),
        ),
        ("/documentation/references/0/excerpt", json!("x".repeat(41))),
        ("/declarations/0/origin/package/version", json!("1.49.0")),
        (
            "/declarations/0/unit",
            json!("rift://source/cargo/crates.io/other/src/lib.rs"),
        ),
    ] {
        let mut changed_result = original.clone();
        *changed_result.pointer_mut(pointer).expect("fixture field") = changed;
        let result = serde_json::from_value(changed_result).expect("typed tampered result");
        assert!(!valid_result(&request, &result, 100), "refuse {pointer}");
    }
    let mut duplicate = original.clone();
    let reference = duplicate["documentation"]["references"][0].clone();
    duplicate["documentation"]["references"]
        .as_array_mut()
        .expect("references")
        .push(reference);
    assert!(!valid_result(
        &request,
        &serde_json::from_value(duplicate).expect("duplicate"),
        100
    ));
    let mut warning = original;
    warning["documentation"]["warnings"] = json!([{
        "source": {"source": {"kind": "package", "unit": "rift://source/cargo/crates.io/other/README.md"}},
        "stage": "source", "kind": "source_unavailable", "count": 1
    }]);
    assert!(!valid_result(
        &request,
        &serde_json::from_value(warning).expect("warning"),
        100
    ));
    request
        .include
        .retain(|include| *include != GetSymbolInclude::Documentation);
    assert!(!valid_result(&request, &result, 100));
}

#[test]
fn canonical_runtime_documentation_preserves_exact_runtime_source_owner() {
    let id = "rift://symbol/stdlib/node/javascript/fs/readFileSync";
    let unit = "rift://source/stdlib/node/doc/api/fs.md";
    let mut value = documented_result();
    value["symbol"]["id"] = json!(id);
    value["symbol"]["language"] = json!("javascript");
    value["symbol"]["name"] = json!("readFileSync");
    value["symbol"]["origin"] = json!({"location": "stdlib", "source_kind": "authored", "runtime": {
        "runtime": "node", "version": "26.11.1"
    }});
    value["declarations"] = json!([]);
    value["revision"] = json!("rift://revision/stdlib/node@26.11.1");
    value["documentation"]["references"][0]["reference"]["target"] = json!(id);
    for pointer in [
        "/documentation/references/0/documentation/source/identity/source/unit",
        "/documentation/references/0/documentation/block/source/source/unit",
    ] {
        *value.pointer_mut(pointer).expect("unit") = json!(unit);
    }
    value["documentation"]["references"][0]["documentation"]["source"]["origin"] = json!({
        "location": "stdlib", "source_kind": "authored", "runtime": {"runtime": "node", "version": "26.11.1"}
    });
    let mut request = request();
    request.id = rift_protocol::read::SymbolId::parse(id).expect("runtime identity");
    request.include.push(GetSymbolInclude::Documentation);
    let result = serde_json::from_value(value.clone()).expect("runtime result");
    assert!(valid_result(&request, &result, 100));
    value["documentation"]["references"][0]["documentation"]["source"]["origin"]["runtime"]["version"] =
        json!("26.11.0");
    let result = serde_json::from_value(value).expect("foreign runtime result");
    assert!(!valid_result(&request, &result, 100));
}
