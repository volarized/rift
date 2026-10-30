use super::*;
use rift_protocol::{
    read::SourceUnitId as CanonicalSourceUnitId,
    source_read::{
        DeclarationPositionResult, FindDeclarationsParams, FindDeclarationsResult,
        PositionEncoding, SourcePosition,
    },
};
use serde_json::{Value, json};

const DECLARATION: &str = "rift://symbol/cargo/crates.io/demo/rust/demo";
const UNIT: &str = "rift://source/cargo/crates.io/demo/src/first.rs";
const RELEASE: &str = "rift://revision/cargo/crates.io/demo@1.0.0";

fn position(line: u64) -> SourcePosition {
    SourcePosition {
        unit: CanonicalSourceUnitId::parse(UNIT).expect("canonical source"),
        revision: Some(rift_protocol::revision::Revision::parse(RELEASE).expect("release")),
        line,
        character: 4,
    }
}

pub(super) fn declaration_request() -> FindDeclarationsParams {
    FindDeclarationsParams {
        position_encoding: PositionEncoding::Utf16,
        positions: vec![position(3), position(0)],
        view: None,
    }
}

fn position_json(line: u64) -> Value {
    serde_json::to_value(position(line)).expect("position JSON")
}

fn view_json() -> Value {
    json!({"id":"a".repeat(64),"expires_at":"2026-10-10T10:05:00Z"})
}

/// One declaration and one definite absence in the same captured selection.
pub(super) fn declaration_response_json() -> Value {
    json!({"results":[
        {"outcome":"found","position":position_json(3),"id":DECLARATION,"kind":"function","revision":RELEASE,"view":view_json()},
        {"outcome":"missing","position":position_json(0),"view":view_json()}
    ]})
}

fn decoded(value: Value) -> FindDeclarationsResult {
    serde_json::from_value(value).expect("declaration response fixture")
}

#[tokio::test]
async fn test_fixture_declarations_answer_each_position_once() {
    let (server, client) = operation_client(OperationFixture::Declarations).await;
    let answer = client
        .find_declarations(&declaration_request())
        .await
        .unwrap_or_else(|error| panic!("declarations: {error:?}"));
    assert!(matches!(&answer.results[..], [
        DeclarationPositionResult::Found { position, id, kind, revision, view },
        DeclarationPositionResult::Missing { position: missing, view: missing_view }
    ] if position.line == 3 && id.as_str() == DECLARATION && kind.0 == "function"
        && revision.to_string() == RELEASE
        && missing.line == 0 && view == missing_view));
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/declarations")
    );
    let body = server
        .state
        .request_log
        .lock()
        .await
        .last()
        .map(|request| request.body.clone())
        .expect("declaration request");
    let sent = serde_json::from_slice::<Value>(&body).expect("request JSON");
    assert_eq!(sent["position_encoding"], json!("utf-16"));
    assert_eq!(sent["positions"][0], position_json(3));
    assert!(sent["positions"][0].get("package").is_none());
    assert!(sent["positions"][0].get("path").is_none());
}

#[tokio::test]
async fn test_fixture_declarations_require_the_advertised_capability() {
    let (server, client) = operation_client(OperationFixture::Valid).await;
    assert_registered_field(
        &client
            .find_declarations(&declaration_request())
            .await
            .expect_err("feature refusal"),
        "rift.client.configuration_invalid",
        "feature",
    );
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/capabilities"),
        "refusal sends no declaration request"
    );
    let client = GlobalClient::new(Config {
        enabled: false,
        ..Config::default()
    })
    .expect("disabled client");
    assert_registered_field(
        &client
            .find_declarations(&declaration_request())
            .await
            .expect_err("disabled client refusal"),
        "rift.client.configuration_invalid",
        "enabled",
    );
}

#[tokio::test]
async fn test_fixture_declaration_refusal_marks_the_endpoint_unavailable() {
    let (_server, client) = operation_client(OperationFixture::Declarations).await;
    let mut request = declaration_request();
    request.positions.truncate(1);
    let error = client
        .find_declarations(&request)
        .await
        .expect_err("position answer refusal");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
    assert!(
        error
            .context()
            .any(|(key, value)| key == "status" && value == "200")
    );
    let cached = client
        .find_declarations(&declaration_request())
        .await
        .expect_err("recorded refusal");
    assert_eq!(
        cached.to_string(),
        error.to_string(),
        "later requests retain the recorded failure"
    );
}

#[tokio::test]
async fn test_declaration_requests_refuse_positions_before_transport() {
    let (server, client) = operation_client(OperationFixture::Declarations).await;
    for field in ["line", "character", "duplicate", "empty", "too_many"] {
        let mut request = declaration_request();
        match field {
            "line" => request.positions[0].line = POSITION_COMPONENT_MAX + 1,
            "character" => request.positions[0].character = POSITION_COMPONENT_MAX + 1,
            "duplicate" => request.positions[0] = request.positions[1].clone(),
            "empty" => request.positions.clear(),
            "too_many" => {
                request.positions = (0..=DECLARATION_POSITIONS_MAX)
                    .map(|line| position(u64::try_from(line).expect("line fits")))
                    .collect();
            }
            _ => unreachable!(),
        }
        assert_registered_field(
            &client.find_declarations(&request).await.expect_err(field),
            "rift.client.request_invalid",
            "positions",
        );
    }
    for unit in [
        "",
        "rift://source/cargo/demo@1.0.0/src/first.rs",
        "rift://source/cargo/crates.io/demo/src/../first.rs",
    ] {
        assert!(CanonicalSourceUnitId::parse(unit).is_err(), "{unit}");
    }
    let mut request = declaration_request();
    request.positions[0].line = POSITION_COMPONENT_MAX;
    request.positions[0].character = POSITION_COMPONENT_MAX;
    assert!(request.is_valid());
    request.positions = (0..DECLARATION_POSITIONS_MAX)
        .map(|line| position(u64::try_from(line).expect("line fits")))
        .collect();
    assert!(request.is_valid());
    assert!(server.state.request_log.lock().await.is_empty());
}

#[tokio::test]
async fn test_declaration_requests_stay_within_the_advertised_package_bound() {
    let (server, client) = operation_client(OperationFixture::Declarations).await;
    client.get_capabilities().await.expect("capabilities");
    client
        .inner
        .capabilities
        .write()
        .await
        .as_mut()
        .expect("cached capabilities")
        .value
        .bounds
        .packages_max = 1;
    let mut request = declaration_request();
    request.positions[1].unit =
        CanonicalSourceUnitId::parse("rift://source/cargo/registry.example/demo/src/first.rs")
            .expect("distinct registry owner");
    assert_registered_field(
        &client
            .find_declarations(&request)
            .await
            .expect_err("owner bound refusal"),
        "rift.client.request_invalid",
        "positions",
    );
    assert!(
        !server
            .state
            .request_log
            .lock()
            .await
            .iter()
            .any(|request| request.path.ends_with("/declarations"))
    );
}

#[test]
fn test_declaration_responses_account_for_every_position() {
    let request = declaration_request();
    assert!(decoded(declaration_response_json()).is_valid_for(&request));
    for change in ["missing", "duplicate", "extra", "reordered", "foreign_view"] {
        let mut value = declaration_response_json();
        match change {
            "missing" => {
                value["results"].as_array_mut().expect("results").pop();
            }
            "duplicate" => value["results"][1]["position"] = position_json(3),
            "extra" => {
                value["results"].as_array_mut().expect("results").push(
                    json!({"outcome":"missing","position":position_json(7),"view":view_json()}),
                );
            }
            "reordered" => value["results"].as_array_mut().expect("results").swap(0, 1),
            "foreign_view" => value["results"][1]["view"]["id"] = json!("b".repeat(64)),
            _ => unreachable!(),
        }
        assert!(!decoded(value).is_valid_for(&request), "{change}");
    }
}

#[tokio::test]
async fn test_declaration_responses_carry_a_well_formed_kind_exactly_beside_a_declaration() {
    let mut invalid_kind = declaration_response_json();
    invalid_kind["results"][0]["kind"] = json!("9function");
    let mut missing_kind = declaration_response_json();
    missing_kind["results"][0]
        .as_object_mut()
        .expect("found")
        .remove("kind");
    let mut absent_kind = declaration_response_json();
    absent_kind["results"][1]["kind"] = json!("function");
    let mut malformed_id = declaration_response_json();
    malformed_id["results"][0]["id"] = json!("rift://symbol/rust/src/first.rs/demo");
    for value in [invalid_kind, missing_kind, absent_kind, malformed_id] {
        let (_server, client) =
            operation_client(OperationFixture::SourceDeclarations(value, StatusCode::OK)).await;
        let error = client
            .find_declarations(&declaration_request())
            .await
            .expect_err("declaration answer refusal");
        assert_registered_field(&error, "rift.client.response_invalid", "body");
        assert!(
            error
                .context()
                .any(|(key, value)| key == "status" && value == "200")
        );
    }
}
