use super::{valid_result, validate_request};
use crate::ClientError;
use rift_protocol::{
    read::GetSymbolInclude,
    symbol_read::{GetSymbolParams, GetSymbolResult},
};
use serde_json::{Value, json};

const ID: &str = "rift://symbol/cargo/crates.io/tokio@1.50.0/rust/tokio/Beacon";
const OTHER_ID: &str = "rift://symbol/cargo/crates.io/tokio@1.49.0/rust/tokio/Beacon";

fn request() -> GetSymbolParams {
    serde_json::from_value(json!({"id": ID})).expect("request")
}

fn view() -> Value {
    json!({"id": "a".repeat(64), "expires_at": "2026-10-10T01:00:00Z"})
}

fn found(declarations: Vec<Value>) -> GetSymbolResult {
    serde_json::from_value(json!({
        "outcome": "found",
        "symbol": {"id": ID, "language": "rust", "name": "Beacon", "kind": "struct"},
        "view": view(),
        "declarations": Value::Array(declarations),
    }))
    .expect("result")
}

fn declaration(source: &str) -> Value {
    json!({
        "origin": {"location": "dependency", "source_kind": "authored", "package": {
            "manager": "cargo", "registry": "crates.io", "name": "tokio", "version": "1.50.0"
        }},
        "unit": "rift://source/cargo/crates.io/tokio@1.50.0/src/lib.rs",
        "range": {"start": 0, "end": source.len()},
        "line": 1,
        "source": source,
        "source_complete": true,
        "signature_indices": [],
        "type_indices": [],
        "documentation_indices": [],
    })
}

#[test]
fn source_less_found_and_typed_absence_remain_distinct() {
    let request = request();
    assert!(valid_result(&request, &found(Vec::new()), 100));
    let missing = serde_json::from_value(json!({
        "outcome": "missing", "view": view(),
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
    request.view =
        Some(rift_protocol::symbol_read::CapturedViewId::parse(&"b".repeat(64)).expect("view"));
    assert!(!valid_result(&request, &found(Vec::new()), 100));
}

#[test]
fn exact_answer_enforces_projection_and_source_bounds() {
    let mut request = request();
    let mut invalid_binding = declaration("pub struct Beacon;");
    invalid_binding["signature_indices"] = json!([0]);
    assert!(!valid_result(&request, &found(vec![invalid_binding]), 100));
    let result = found(vec![declaration("pub struct Beacon;")]);
    assert!(valid_result(&request, &result, 100));
    assert!(!valid_result(&request, &result, 5));
    request.include.clear();
    assert!(!valid_result(&request, &result, 100));
    request.include.push(GetSymbolInclude::Source);
    request.declaration_limit = 1;
    let result = found(vec![
        declaration("pub struct Beacon;"),
        declaration("struct Beacon;"),
    ]);
    assert!(!valid_result(&request, &result, 100));
}

#[test]
fn request_rejects_local_owners_revision_and_unbound_continuation() {
    let local: GetSymbolParams = serde_json::from_value(json!({
        "id": "rift://symbol/local/rust/Beacon"
    }))
    .expect("local request");
    assert!(matches!(
        validate_request(&local),
        Err(ClientError::InvalidRequest { field: "id" })
    ));
    let mut request = request();
    request.declaration_cursor = Some("next".to_owned());
    assert!(matches!(
        validate_request(&request),
        Err(ClientError::InvalidRequest {
            field: "declaration_cursor"
        })
    ));
    request.declaration_cursor = None;
    request.declaration_limit = 0;
    assert!(matches!(
        validate_request(&request),
        Err(ClientError::InvalidRequest {
            field: "declaration_limit"
        })
    ));
    request.declaration_limit = 5;
    request.rev = Some(rift_protocol::read::RevisionId("main".to_owned()));
    assert!(matches!(
        validate_request(&request),
        Err(ClientError::InvalidRequest { field: "rev" })
    ));
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
    assert!(matches!(
        crate::response::exact_symbol(response),
        Err(ClientError::Decode { status: 200 })
    ));
}

#[test]
fn exact_response_requires_status_to_match_outcome() {
    let outcomes = [
        (200, serde_json::to_value(found(Vec::new())).expect("found")),
        (
            404,
            json!({"outcome":"missing","symbol_not_found":{"id":ID,"alternatives":[]},"view":view()}),
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
                assert!(
                    matches!(result, Err(ClientError::InvalidResponse { status: actual }) if actual == status)
                );
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
        assert!(
            matches!(crate::response::exact_symbol(response), Err(ClientError::Http { problem, .. }) if problem.is_some() == (media == "application/problem+json"))
        );
    }
}
