use super::{OperationFixture, StatusCode, assert_registered_field, operation_client};
use rift_protocol::{
    read::{NodesParams, SymbolId},
    revision::Revision,
    symbol_read::GetSymbolParams,
};
use serde_json::{Value, json};

const ID: &str = "rift://symbol/stdlib/cpython/python/json/dumps";
const RELEASE: &str = "rift://revision/stdlib/cpython@3.13.0";
const IMPLEMENTATION: &str = "rift://source/stdlib/cpython/json/__init__.py";
const STUB: &str = "rift://source/external/typeshed-0123456789abcdef0123456789abcdef01234567/stdlib/json/__init__.pyi";
const OTHER_RESOLVER: &str = "rift://source/project/typeshed-0123456789abcdef0123456789abcdef01234567/stdlib/json/__init__.pyi";
const UNPINNED: &str = "rift://source/external/typeshed-main/stdlib/json/__init__.pyi";
const OTHER_PATH: &str = "rift://source/external/typeshed-0123456789abcdef0123456789abcdef01234567/stubs/json/__init__.pyi";

fn declaration(origin: &Value, unit: &str) -> Value {
    json!({
        "origin": origin, "unit": unit, "range": {"start":0,"end":5}, "line":1,
        "signature_indices":[], "type_indices":[], "documentation_indices":[],
    })
}

fn symbol_response() -> Value {
    let runtime = json!({
        "location":"stdlib", "source_kind":"authored",
        "runtime":{"runtime":"cpython", "version":"3.13.0"},
    });
    let external = json!({"location":"external", "source_kind":"authored"});
    json!({
        "outcome":"found", "revision":RELEASE,
        "symbol":{"id":ID,"name":"dumps","kind":"function","language":"python","origin":runtime},
        "declarations":[declaration(&runtime, IMPLEMENTATION), declaration(&external, STUB)],
    })
}

#[tokio::test]
async fn runtime_declarations_include_only_the_pinned_typeshed_stub() {
    let request: GetSymbolParams =
        serde_json::from_value(json!({"id":ID,"revision":RELEASE})).expect("exact runtime request");
    let expected = symbol_response();
    let (_server, client) = operation_client(OperationFixture::ExactSymbol(expected.clone())).await;
    let answer = client
        .get_symbol(&request)
        .await
        .expect("paired declarations");
    assert_eq!(serde_json::to_value(answer).expect("answer"), expected);

    for (field, value) in [
        ("/declarations/1/unit", json!(OTHER_RESOLVER)),
        ("/declarations/1/unit", json!(UNPINNED)),
        ("/declarations/1/unit", json!(OTHER_PATH)),
        (
            "/declarations/1/origin",
            expected["symbol"]["origin"].clone(),
        ),
        ("/revision", json!("rift://revision/stdlib/cpython@3.12.0")),
        (
            "/symbol/id",
            json!("rift://symbol/stdlib/pypy/python/json/dumps"),
        ),
    ] {
        let mut invalid = expected.clone();
        *invalid.pointer_mut(field).expect("fixture field") = value;
        let (_server, client) = operation_client(OperationFixture::ExactSymbol(invalid)).await;
        let error = client.get_symbol(&request).await.expect_err(field);
        assert_registered_field(&error, "rift.client.response_invalid", "body");
    }

    let other_id = "rift://symbol/stdlib/pypy/python/json/dumps";
    let other_release = "rift://revision/stdlib/pypy@3.13.0";
    let other_request: GetSymbolParams =
        serde_json::from_value(json!({"id":other_id,"revision":other_release}))
            .expect("different runtime request");
    let mut other = expected;
    other["revision"] = json!(other_release);
    other["symbol"]["id"] = json!(other_id);
    other["symbol"]["origin"]["runtime"]["runtime"] = json!("pypy");
    other["declarations"][0]["origin"] = other["symbol"]["origin"].clone();
    other["declarations"][0]["unit"] = json!("rift://source/stdlib/pypy/json/__init__.py");
    let (_server, client) = operation_client(OperationFixture::ExactSymbol(other)).await;
    let error = client
        .get_symbol(&other_request)
        .await
        .expect_err("unrelated runtime with stub");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
}

fn node_response(unit: &str) -> Value {
    json!({"symbol":ID,"revision":RELEASE,"nodes":[{
        "declaration":1,"index":0,"unit":unit,"kind":"identifier","language":"python",
        "range":{"start":0,"end":5},"source":"dumps",
    }]})
}

#[tokio::test]
async fn runtime_node_continuation_accepts_the_pinned_typeshed_stub() {
    let request = NodesParams {
        revision: Some(Revision::parse(RELEASE).expect("exact runtime revision")),
        node_cursor: Some("continue".to_owned()),
        ..NodesParams::of(SymbolId(ID.to_owned()))
    };
    let (_server, client) =
        operation_client(OperationFixture::Nodes(node_response(STUB), StatusCode::OK)).await;
    let answer = client.nodes(&request).await.expect("Typeshed node page");
    assert_eq!(answer.nodes[0].unit.as_str(), STUB);
    assert_eq!(answer.nodes[0].source, "dumps");

    for (field, value) in [
        ("/nodes/0/unit", json!(OTHER_RESOLVER)),
        ("/nodes/0/unit", json!(UNPINNED)),
        ("/nodes/0/unit", json!(OTHER_PATH)),
        ("/nodes/0/language", json!("rust")),
        (
            "/nodes/0/unit",
            json!("rift://source/stdlib/pypy/json/__init__.py"),
        ),
        ("/revision", json!("rift://revision/stdlib/cpython@3.12.0")),
    ] {
        let mut invalid = node_response(STUB);
        *invalid.pointer_mut(field).expect("fixture field") = value;
        let (_server, client) =
            operation_client(OperationFixture::Nodes(invalid, StatusCode::OK)).await;
        let error = client.nodes(&request).await.expect_err(field);
        assert_registered_field(&error, "rift.client.response_invalid", "body");
    }
}
