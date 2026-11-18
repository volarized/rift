use super::*;
use rift_protocol::read::{FACETS_SOURCE_BYTES_MAX, FacetsParams, FacetsResult, SymbolId};
use serde_json::{Value, json};

const SYMBOL: &str = "rift://symbol/cargo/crates.io/demo/rust/demo/beacon";
const UNIT: &str = "rift://source/cargo/crates.io/demo/src/lib.rs";
const RELEASE: &str = "rift://revision/cargo/crates.io/demo@1.0.0";

fn revision(value: &str) -> rift_protocol::revision::Revision {
    rift_protocol::revision::Revision::parse(value).expect("revision fixture")
}

pub(super) fn facets_request() -> FacetsParams {
    FacetsParams::of(SymbolId(SYMBOL.to_owned()))
}

fn syntax_element_json(index: u64, kind: &str, start: u64, source: &str) -> Value {
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

/// The facets of `beacon` in `pub fn helper_beacon() {}\npub fn beacon() {}\n`: its item syntax element,
/// then the item's own descendants.
pub(super) fn facets_response_json() -> Value {
    let mut item = syntax_element_json(0, "function_item", 26, "pub fn beacon() {}");
    item["facets"] = json!(["declaration", "definition"]);
    facet_response(&[
        item,
        syntax_element_json(2, "identifier", 33, "beacon"),
        syntax_element_json(4, "block", 42, "{}"),
    ])
}

/// One item syntax element of `beacon` whose source holds `source_bytes` bytes.
pub(super) fn single_syntax_element_response_json(source_bytes: usize) -> Value {
    let item = syntax_element_json(0, "function_item", 0, &"x".repeat(source_bytes));
    facet_response(&[item])
}

fn facet_response(syntax: &[Value]) -> Value {
    let declarations = syntax.first().map(|root| json!({
        "declaration": root["declaration"], "kind": root["kind"], "range": root["range"],
        "source": {"span": {"unit": root["unit"], "range": root["range"]}, "text": root["source"]},
        "syntax": syntax,
    })).into_iter().collect::<Vec<_>>();
    json!({"symbol": SYMBOL, "revision": RELEASE, "pagination": {"page":1,"pages":u64::from(!declarations.is_empty()),"max_results":10,"total":declarations.len()}, "declarations": declarations})
}

/// Sets the declaration's ordinal and every syntax element's owner together.
fn set_ordinal(declaration: &mut Value, ordinal: u64) {
    declaration["declaration"] = json!(ordinal);
    for element in declaration["syntax"]
        .as_array_mut()
        .expect("fixture syntax")
    {
        element["declaration"] = json!(ordinal);
    }
}

/// One named change to a valid facets answer.
type AnswerMutation = (&'static str, fn(&mut Value));

async fn facets_answer(value: Value) -> Result<FacetsResult, RiftError> {
    let (_server, client) = operation_client(OperationFixture::Facets(value, StatusCode::OK)).await;
    client.facets(&facets_request()).await
}

#[tokio::test]
async fn test_fixture_facets_list_a_package_symbol_from_its_symbol_alone() {
    let (server, client) = operation_client(OperationFixture::Facets(
        facets_response_json(),
        StatusCode::OK,
    ))
    .await;
    let answer = client
        .facets(&facets_request())
        .await
        .unwrap_or_else(|error| panic!("facets: {error:?}"));
    let expected: FacetsResult =
        serde_json::from_value(facets_response_json()).expect("facets response fixture");
    assert_eq!(answer, expected);
    assert!(
        answer.declarations[0]
            .syntax
            .iter()
            .all(|syntax_element| syntax_element.unit.as_str() == UNIT)
    );
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/facets")
    );
    let body = server
        .state
        .request_log
        .lock()
        .await
        .last()
        .map(|request| request.body.clone())
        .expect("facets request");
    let sent = serde_json::from_slice::<Value>(&body).expect("request JSON");
    assert_eq!(sent, json!({"symbol": SYMBOL,"page":1,"max_results":10}));
}

/// The global API serves the facets of one package or runtime symbol: a request in any other
/// form refuses, naming its field, before it reaches the endpoint.
#[tokio::test]
async fn facets_requests_outside_the_served_form_refuse_before_any_request() {
    let cases = [
        (
            "symbol",
            FacetsParams::of(SymbolId("rift://symbol/local/rust/demo/beacon".to_owned())),
        ),
        (
            "revision",
            FacetsParams {
                revision: Some(revision("rift://revision/git/main")),
                ..facets_request()
            },
        ),
        (
            "revision",
            FacetsParams {
                revision: Some(revision("rift://revision/cargo/crates.io/other@1.0.0")),
                ..facets_request()
            },
        ),
        (
            "page",
            FacetsParams {
                page: 0,
                ..facets_request()
            },
        ),
        (
            "max_results",
            FacetsParams {
                max_results: rift_protocol::read::FACETS_MAX_RESULTS + 1,
                ..facets_request()
            },
        ),
    ];
    let (server, client) = operation_client(OperationFixture::Facets(
        facets_response_json(),
        StatusCode::OK,
    ))
    .await;
    for (field, request) in cases {
        let error = client.facets(&request).await.expect_err("refused request");
        assert_registered_field(&error, "rift.client.request_invalid", field);
    }
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_symbol_the_release_does_not_declare_refuses_symbol_not_found() {
    let (_server, client) =
        operation_client(OperationFixture::Facets(Value::Null, StatusCode::NOT_FOUND)).await;
    let error = client
        .facets(&facets_request())
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
    let (server, client) = operation_client(OperationFixture::Facets(
        facets_response_json(),
        StatusCode::OK,
    ))
    .await;
    let request = FacetsParams {
        max_results: rift_protocol::read::FACETS_MAX_RESULTS + 1,
        ..facets_request()
    };
    let error = client
        .facets(&request)
        .await
        .expect_err("cursor past the bound");
    assert_registered_field(&error, "rift.client.request_invalid", "max_results");
    assert_eq!(server.state.last_path.lock().await.as_deref(), None);
    let disabled = GlobalClient::new(Config {
        enabled: false,
        ..Config::default()
    })
    .expect("disabled client");
    let error = disabled
        .facets(&facets_request())
        .await
        .expect_err("disabled client refusal");
    assert_registered_field(&error, "rift.client.configuration_invalid", "enabled");
}

#[tokio::test]
async fn a_global_api_without_the_facets_feature_refuses_before_the_read() {
    let (server, client) = operation_client(OperationFixture::Declarations).await;
    let error = client
        .facets(&facets_request())
        .await
        .expect_err("unadvertised feature");
    assert_registered_field(&error, "rift.client.configuration_invalid", "feature");
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
}

/// Each answer naming a syntax element outside the symbol's release, or one whose bytes or page break
/// the contract, refuses as an invalid response.
#[tokio::test]
async fn facets_breaking_the_contract_refuse_as_invalid_responses() {
    let mutations: [AnswerMutation; 8] = [
        ("project file unit", |answer| {
            answer["declarations"][0]["syntax"][1]["unit"] =
                json!("rift://source/project/src/lib.rs");
        }),
        ("unit of another package", |answer| {
            answer["declarations"][0]["syntax"][1]["unit"] =
                json!("rift://source/cargo/crates.io/other/src/lib.rs");
        }),
        ("another symbol", |answer| {
            answer["symbol"] = json!("rift://symbol/cargo/crates.io/demo/rust/demo/helper_beacon");
        }),
        ("source shorter than its range", |answer| {
            answer["declarations"][0]["syntax"][1]["source"] = json!("beaco");
        }),
        ("source differs from the declaration", |answer| {
            answer["declarations"][0]["syntax"][1]["source"] = json!("BEACON");
        }),
        ("cursor beside no facets", |answer| {
            *answer = json!({"symbol": SYMBOL, "revision": RELEASE, "declarations": [], "max_results": "1.ab"});
        }),
        ("empty cursor", |answer| {
            answer["max_results"] = json!("");
        }),
        ("page over the source bound", |answer| {
            let half = usize::try_from(FACETS_SOURCE_BYTES_MAX / 2 + 1).expect("bound fits usize");
            let one = single_syntax_element_response_json(half);
            let mut second = one["declarations"][0].clone();
            set_ordinal(&mut second, 1);
            answer["declarations"] = json!([one["declarations"][0].clone(), second]);
        }),
    ];
    for (case, mutate) in mutations {
        let mut answer = facets_response_json();
        mutate(&mut answer);
        let error = facets_answer(answer).await.expect_err(case);
        assert_registered_field(&error, "rift.client.response_invalid", "body");
    }
}

#[tokio::test]
async fn a_syntax_element_over_the_source_bound_refuses_and_one_at_it_passes() {
    let at_bound = single_syntax_element_response_json(SOURCE_BYTES_MAX);
    let answer = facets_answer(at_bound)
        .await
        .expect("one syntax element at the source bound");
    assert_eq!(answer.declarations.len(), 1);
    let over = single_syntax_element_response_json(SOURCE_BYTES_MAX + 1);
    let error = facets_answer(over)
        .await
        .expect_err("syntax element over the source bound");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
}

#[tokio::test]
async fn a_page_over_the_advertised_declaration_bound_refuses() {
    let (_server, client) =
        operation_client(OperationFixture::Facets(Value::Null, StatusCode::OK)).await;
    let advertised = client
        .get_capabilities()
        .await
        .expect("fixture capabilities")
        .bounds
        .page_limit_max;
    let count = usize::try_from(advertised).expect("advertised bound fits usize") + 1;
    let declarations = (0..count)
        .map(|ordinal| {
            let mut declaration = facets_response_json()["declarations"][0].clone();
            set_ordinal(&mut declaration, ordinal as u64);
            declaration
        })
        .collect::<Vec<_>>();
    let error =
        facets_answer(json!({"symbol": SYMBOL, "revision": RELEASE, "pagination": {"page":1,"pages":u64::from(!declarations.is_empty()),"max_results":10,"total":declarations.len()}, "declarations": declarations}))
            .await
            .expect_err("page over the declaration bound");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
}

/// A request naming one release refuses an answer read at another release of the package.
#[tokio::test]
async fn an_answer_at_another_release_than_the_requested_one_refuses() {
    let request = FacetsParams {
        revision: Some(revision("rift://revision/cargo/crates.io/demo@2.0.0")),
        ..facets_request()
    };
    let (_server, client) = operation_client(OperationFixture::Facets(
        facets_response_json(),
        StatusCode::OK,
    ))
    .await;
    let error = client.facets(&request).await.expect_err("another release");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
}
