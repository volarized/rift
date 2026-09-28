use super::*;
use crate::declaration::{
    DECLARATION_POSITIONS_MAX, POSITION_COMPONENT_MAX, validate_declaration_request,
    validate_declaration_request_for_capabilities, validate_declaration_response,
};
use serde_json::{Value, json};

const DECLARATION: &str = "rift://symbol/rust/src/first.rs/demo";

/// One edit to a submitted position, beside the field the client names when it refuses it.
type PositionEdit = (fn(&mut PackagePosition), &'static str);

fn position(line: i64) -> PackagePosition {
    PackagePosition {
        package: package_request(),
        path: "src/first.rs".to_owned(),
        line,
        character: 4,
    }
}

fn declaration_request() -> PackageDeclarationRequest {
    PackageDeclarationRequest {
        position_encoding: PackageDeclarationRequestPositionEncoding::Utf16,
        positions: vec![position(3), position(0)],
    }
}

fn position_json(line: i64) -> Value {
    serde_json::to_value(position(line)).expect("position JSON")
}

/// The fixture's answer: a declaration for the call on line 3, none for the comment on line 0.
pub(super) fn declaration_response_json() -> Value {
    json!({"results": [
        {"position": position_json(3), "declaration": DECLARATION},
        {"position": position_json(0)}
    ]})
}

fn decoded(value: Value) -> PackageDeclarationResponse {
    serde_json::from_value(value).expect("declaration response fixture")
}

fn declaration_capabilities() -> Capabilities {
    let mut capabilities: Capabilities =
        serde_json::from_str(&capabilities_json()).expect("capabilities fixture");
    capabilities
        .supported_features
        .push("declarations".to_owned());
    capabilities
}

#[tokio::test]
async fn test_fixture_declarations_answer_each_position_once() {
    let (server, client) = operation_client(OperationFixture::Declarations).await;
    let answer = client
        .find_package_declarations(&declaration_request())
        .await
        .unwrap_or_else(|error| panic!("declarations: {error:?}"));
    let declarations = answer
        .results
        .iter()
        .map(|result| (result.position.line, result.declaration.as_deref()))
        .collect::<Vec<_>>();
    assert_eq!(declarations, [(3, Some(DECLARATION)), (0, None)]);
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
        .expect("the declaration request");
    let sent = serde_json::from_slice::<Value>(&body).expect("request JSON");
    assert_eq!(sent["position_encoding"], json!("utf-16"));
    assert_eq!(sent["positions"][0], position_json(3));
}

#[tokio::test]
async fn test_fixture_declarations_require_the_advertised_capability() {
    let (server, client) = operation_client(OperationFixture::Valid).await;
    assert_eq!(
        client
            .find_package_declarations(&declaration_request())
            .await,
        Err(ClientError::InvalidRequest { field: "positions" })
    );
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/capabilities"),
        "the refusal sends no declaration request"
    );

    let client = GlobalClient::new(Config {
        enabled: false,
        ..Config::default()
    })
    .expect("disabled client");
    assert_eq!(
        client
            .find_package_declarations(&declaration_request())
            .await,
        Err(ClientError::Disabled)
    );
}

#[tokio::test]
async fn test_fixture_declaration_refusal_marks_the_endpoint_unavailable() {
    let (_server, client) = operation_client(OperationFixture::Declarations).await;
    let mut request = declaration_request();
    request.positions.truncate(1);
    assert_eq!(
        client.find_package_declarations(&request).await,
        Err(ClientError::InvalidResponseField {
            field: "position_accounting"
        })
    );
    assert_eq!(
        client
            .find_package_declarations(&declaration_request())
            .await,
        Err(ClientError::Connection)
    );
}

#[test]
fn test_declaration_requests_refuse_positions_before_transport() {
    assert_eq!(validate_declaration_request(&declaration_request()), Ok(()));
    let with = |edit: fn(&mut PackagePosition)| {
        let mut request = declaration_request();
        edit(&mut request.positions[0]);
        validate_declaration_request(&request)
    };
    let cases: [PositionEdit; 8] = [
        (|position| position.path = String::new(), "path"),
        (
            |position| position.path = "/src/first.rs".to_owned(),
            "path",
        ),
        (
            |position| position.path = "src/../first.rs".to_owned(),
            "path",
        ),
        (|position| position.line = -1, "line"),
        (
            |position| position.line = POSITION_COMPONENT_MAX + 1,
            "line",
        ),
        (|position| position.character = -1, "character"),
        (
            |position| position.package.version = String::new(),
            "package_version",
        ),
        (|position| position.line = 0, "duplicate_position"),
    ];
    for (edit, field) in cases {
        assert_eq!(
            with(edit),
            Err(ClientError::InvalidRequest { field }),
            "{field}"
        );
    }
    assert_eq!(
        with(|position| {
            position.line = POSITION_COMPONENT_MAX;
            position.character = POSITION_COMPONENT_MAX;
        }),
        Ok(())
    );

    let mut request = declaration_request();
    request.positions.clear();
    assert_eq!(
        validate_declaration_request(&request),
        Err(ClientError::InvalidRequest { field: "positions" })
    );
    request.positions = (0..=DECLARATION_POSITIONS_MAX)
        .map(|line| position(i64::try_from(line).expect("line fits")))
        .collect();
    assert_eq!(
        validate_declaration_request(&request),
        Err(ClientError::InvalidRequest { field: "positions" })
    );
    request.positions.pop();
    assert_eq!(validate_declaration_request(&request), Ok(()));
}

#[test]
fn test_declaration_requests_stay_within_the_advertised_package_bound() {
    let mut capabilities = declaration_capabilities();
    let mut request = declaration_request();
    request.positions[1].package.name = "other".to_owned();
    assert_eq!(
        validate_declaration_request_for_capabilities(&request, &capabilities),
        Ok(())
    );
    capabilities.bounds.packages_max = 1;
    assert_eq!(
        validate_declaration_request_for_capabilities(&request, &capabilities),
        Err(ClientError::InvalidRequest { field: "packages" })
    );
}

#[test]
fn test_declaration_responses_account_for_every_position() {
    let request = declaration_request();
    assert_eq!(
        validate_declaration_response(&request, &decoded(declaration_response_json())),
        Ok(())
    );
    let cases = [
        json!({"results": [{"position": position_json(3), "declaration": DECLARATION}]}),
        json!({"results": [
            {"position": position_json(3)}, {"position": position_json(3)}, {"position": position_json(0)}
        ]}),
        json!({"results": [
            {"position": position_json(3)}, {"position": position_json(0)}, {"position": position_json(7)}
        ]}),
    ];
    for response in cases {
        assert_eq!(
            validate_declaration_response(&request, &decoded(response)),
            Err(ClientError::InvalidResponseField {
                field: "position_accounting"
            })
        );
    }
    let invalid = json!({"results": [
        {"position": position_json(3), "declaration": "rift://symbol/rust/src/../first.rs/demo"},
        {"position": position_json(0)}
    ]});
    assert_eq!(
        validate_declaration_response(&request, &decoded(invalid)),
        Err(ClientError::InvalidResponseField {
            field: "declaration"
        })
    );
}
