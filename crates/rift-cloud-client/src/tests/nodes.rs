use super::*;
use rift_protocol::read::{NODES_SOURCE_BYTES_MAX, NodeUnit, NodesParams, NodesResult, SymbolId};
use serde_json::{Value, json};

const SYMBOL: &str = "rift://symbol/cargo/crates.io/demo@1.0.0/rust/demo/beacon";
const UNIT: &str = "rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs";

pub(super) fn nodes_request() -> NodesParams {
    NodesParams::of(SymbolId(SYMBOL.to_owned()))
}

fn node_json(index: u64, kind: &str, start: u64, source: &str) -> Value {
    let end = start + u64::try_from(source.len()).expect("fixture source length fits u64");
    json!({
        "declaration": 0,
        "index": index,
        "unit": UNIT,
        "language": "rust",
        "kind": kind,
        "range": {"start": start, "end": end},
        "source": source
    })
}

/// The nodes of `beacon` in `pub fn helper_beacon() {}\npub fn beacon() {}\n`: its item node,
/// then the item's own descendants.
pub(super) fn nodes_response_json() -> Value {
    let mut item = node_json(0, "function_item", 26, "pub fn beacon() {}");
    item["facets"] = json!(["declaration", "definition"]);
    json!({"symbol": SYMBOL, "nodes": [
        item,
        node_json(
            2,
            "identifier",
            33,
            "beacon",
        ),
        node_json(
            4,
            "block",
            42,
            "{}",
        ),
    ]})
}

/// One item node of `beacon` whose source holds `source_bytes` bytes.
pub(super) fn single_node_response_json(source_bytes: usize) -> Value {
    let item = node_json(0, "function_item", 0, &"x".repeat(source_bytes));
    json!({"symbol": SYMBOL, "nodes": [item]})
}

/// One named change to a valid nodes answer.
type AnswerMutation = (&'static str, fn(&mut Value));

async fn nodes_answer(value: Value) -> Result<NodesResult, RiftError> {
    let (_server, client) = operation_client(OperationFixture::Nodes(value, StatusCode::OK)).await;
    client.nodes(&nodes_request()).await
}

#[tokio::test]
async fn test_fixture_nodes_list_a_package_symbol_from_its_symbol_alone() {
    let (server, client) = operation_client(OperationFixture::Nodes(
        nodes_response_json(),
        StatusCode::OK,
    ))
    .await;
    let answer = client
        .nodes(&nodes_request())
        .await
        .unwrap_or_else(|error| panic!("nodes: {error:?}"));
    let expected: NodesResult =
        serde_json::from_value(nodes_response_json()).expect("nodes response fixture");
    assert_eq!(answer, expected);
    assert!(
        answer
            .nodes
            .iter()
            .all(|node| matches!(&node.unit, NodeUnit::Source(unit) if unit.as_str() == UNIT))
    );
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/nodes")
    );
    let body = server
        .state
        .request_log
        .lock()
        .await
        .last()
        .map(|request| request.body.clone())
        .expect("nodes request");
    let sent = serde_json::from_slice::<Value>(&body).expect("request JSON");
    assert_eq!(sent, json!({"symbol": SYMBOL}));
}

/// The global API serves the nodes of one package or runtime symbol: a request in any other
/// form refuses, naming its field, before it reaches the endpoint.
#[tokio::test]
async fn nodes_requests_outside_the_served_form_refuse_before_any_request() {
    let path = rift_protocol::read::ProjectPath("src/lib.rs".to_owned());
    let cases = [
        ("symbol", NodesParams::at(path.clone(), 0)),
        (
            "symbol",
            NodesParams::of(SymbolId("rift://symbol/local/rust/demo/beacon".to_owned())),
        ),
        (
            "path",
            NodesParams {
                path: Some(path),
                ..nodes_request()
            },
        ),
        (
            "position",
            NodesParams {
                position: Some(0),
                ..nodes_request()
            },
        ),
        (
            "rev",
            NodesParams {
                rev: Some(rift_protocol::read::RevisionId("main".to_owned())),
                ..nodes_request()
            },
        ),
        (
            "node_cursor",
            NodesParams {
                node_cursor: Some(String::new()),
                ..nodes_request()
            },
        ),
        (
            "node_cursor",
            NodesParams {
                node_cursor: Some("c".repeat(CURSOR_BYTES_MAX + 1)),
                ..nodes_request()
            },
        ),
    ];
    let (server, client) = operation_client(OperationFixture::Nodes(
        nodes_response_json(),
        StatusCode::OK,
    ))
    .await;
    for (field, request) in cases {
        let error = client.nodes(&request).await.expect_err("refused request");
        assert_registered_field(&error, "rift.client.request_invalid", field);
    }
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_symbol_the_release_does_not_declare_refuses_symbol_not_found() {
    let (_server, client) =
        operation_client(OperationFixture::Nodes(Value::Null, StatusCode::NOT_FOUND)).await;
    let error = client
        .nodes(&nodes_request())
        .await
        .expect_err("absent symbol");
    assert_eq!(
        error.slug(),
        rift_error::errors::server::read_symbol_not_found::SLUG
    );
    assert_eq!(error_text_field(&error, "id"), SYMBOL);
}

/// A cursor past the advertised bound refuses after the capability read alone, and a
/// disabled client refuses before any request.
#[tokio::test]
async fn a_cursor_past_the_advertised_bound_and_a_disabled_client_refuse() {
    let (server, client) = operation_client(OperationFixture::Nodes(
        nodes_response_json(),
        StatusCode::OK,
    ))
    .await;
    let request = NodesParams {
        node_cursor: Some("c".repeat(NODES_CURSOR_BYTES_ADVERTISED + 1)),
        ..nodes_request()
    };
    let error = client
        .nodes(&request)
        .await
        .expect_err("cursor past the bound");
    assert_registered_field(&error, "rift.client.request_invalid", "node_cursor");
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/capabilities")
    );
    let disabled = GlobalClient::new(Config {
        enabled: false,
        ..Config::default()
    })
    .expect("disabled client");
    let error = disabled
        .nodes(&nodes_request())
        .await
        .expect_err("disabled client refusal");
    assert_registered_field(&error, "rift.client.configuration_invalid", "enabled");
}

#[tokio::test]
async fn a_global_api_without_the_nodes_feature_refuses_before_the_read() {
    let (server, client) = operation_client(OperationFixture::Declarations).await;
    let error = client
        .nodes(&nodes_request())
        .await
        .expect_err("unadvertised feature");
    assert_registered_field(&error, "rift.client.configuration_invalid", "feature");
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
}

/// Each answer naming a node outside the symbol's release, or one whose bytes or page break
/// the contract, refuses as an invalid response.
#[tokio::test]
async fn nodes_breaking_the_contract_refuse_as_invalid_responses() {
    let mutations: [AnswerMutation; 7] = [
        ("project file unit", |answer| {
            answer["nodes"][1]["unit"] = json!("rift://file/src/lib.rs");
        }),
        ("unit of another release", |answer| {
            answer["nodes"][1]["unit"] =
                json!("rift://source/cargo/crates.io/demo@2.0.0/src/lib.rs");
        }),
        ("another symbol", |answer| {
            answer["symbol"] =
                json!("rift://symbol/cargo/crates.io/demo@1.0.0/rust/demo/helper_beacon");
        }),
        ("source shorter than its range", |answer| {
            answer["nodes"][1]["source"] = json!("beaco");
        }),
        ("cursor beside no nodes", |answer| {
            *answer = json!({"nodes": [], "node_cursor": "1.ab"});
        }),
        ("empty cursor", |answer| {
            answer["node_cursor"] = json!("");
        }),
        ("page over the source bound", |answer| {
            let half = usize::try_from(NODES_SOURCE_BYTES_MAX / 2 + 1).expect("bound fits usize");
            answer["nodes"] = json!([
                node_json(0, "line_comment", 0, &"a".repeat(half)),
                node_json(1, "line_comment", 0, &"b".repeat(half)),
            ]);
        }),
    ];
    for (case, mutate) in mutations {
        let mut answer = nodes_response_json();
        mutate(&mut answer);
        let error = nodes_answer(answer).await.expect_err(case);
        assert_registered_field(&error, "rift.client.response_invalid", "body");
    }
}

#[tokio::test]
async fn a_node_over_the_source_bound_refuses_and_one_at_it_passes() {
    let at_bound =
        json!({"nodes": [node_json(0, "line_comment", 0, &"a".repeat(SOURCE_BYTES_MAX))]});
    let answer = nodes_answer(at_bound)
        .await
        .expect("one node at the source bound");
    assert_eq!(answer.nodes.len(), 1);
    let over =
        json!({"nodes": [node_json(0, "line_comment", 0, &"a".repeat(SOURCE_BYTES_MAX + 1))]});
    let error = nodes_answer(over)
        .await
        .expect_err("node over the source bound");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
}

#[tokio::test]
async fn a_page_over_the_advertised_node_bound_refuses() {
    let (_server, client) =
        operation_client(OperationFixture::Nodes(Value::Null, StatusCode::OK)).await;
    let advertised = client
        .get_capabilities()
        .await
        .expect("fixture capabilities")
        .bounds
        .page_limit_max;
    let count = usize::try_from(advertised).expect("advertised bound fits usize") + 1;
    let nodes = (0..count)
        .map(|_| node_json(2, "identifier", 33, "beacon"))
        .collect::<Vec<_>>();
    let error = nodes_answer(json!({"nodes": nodes}))
        .await
        .expect_err("page over the node bound");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
}

/// A request naming a view accepts only the answer carrying that view.
#[tokio::test]
async fn an_answer_for_another_view_refuses() {
    let view =
        rift_protocol::symbol_read::CapturedViewId::parse(&"a".repeat(64)).expect("view fixture");
    let request = NodesParams {
        view: Some(view),
        ..nodes_request()
    };
    let mut answer = nodes_response_json();
    answer["view"] = json!({"id": "b".repeat(64), "expires_at": "2026-10-10T10:05:00Z"});
    let (_server, client) =
        operation_client(OperationFixture::Nodes(answer.clone(), StatusCode::OK)).await;
    let error = client.nodes(&request).await.expect_err("another view");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
    answer["view"]["id"] = json!("a".repeat(64));
    let (_server, client) = operation_client(OperationFixture::Nodes(answer, StatusCode::OK)).await;
    let answered = client.nodes(&request).await.expect("the requested view");
    assert!(answered.view.is_some());
}
