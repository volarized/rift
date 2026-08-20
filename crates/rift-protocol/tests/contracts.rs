//! Tool contract metadata stays aligned with the protocol request models.

use std::error::Error;

use rift_protocol::contracts::TOOL_CONTRACTS;
use rift_protocol::read::{GetSymbolParams, SearchParams, SearchScope};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The typed `get_symbol` request every example derives from.
fn example_get_symbol_request() -> GetSymbolParams {
    GetSymbolParams {
        name: "ReadService".to_owned(),
        language: None,
        include_body: true,
        include_history: false,
        limit: 5,
        cursor: None,
        projection: None,
        scope: SearchScope::All,
    }
}

/// The typed `search` request every example derives from.
fn example_search_request() -> SearchParams {
    SearchParams {
        query: Some("ReadService".to_owned()),
        ..minimal_search_request()
    }
}

fn minimal_search_request() -> SearchParams {
    serde_json::from_value(json!({})).expect("every search parameter has a serde default")
}

#[test]
fn contracts_name_exactly_the_shipped_tools() {
    let tools = TOOL_CONTRACTS
        .iter()
        .map(|contract| (contract.name, contract.request_model, contract.result_model))
        .collect::<Vec<_>>();

    assert_eq!(
        tools,
        [
            ("get_symbol", "GetSymbolParams", "GetSymbolResult"),
            ("search", "SearchParams", "SearchResult"),
        ]
    );
}

#[test]
fn contracts_carry_tool_descriptions() {
    for contract in TOOL_CONTRACTS {
        assert!(
            !contract.description.trim().is_empty(),
            "tool {} must carry the description an agent sees",
            contract.name
        );
    }
}

#[test]
fn example_requests_serialize_from_typed_models() -> TestResult {
    let get_symbol = serde_json::to_value(example_get_symbol_request())?;
    assert_eq!(get_symbol["name"], "ReadService");
    assert_eq!(get_symbol["limit"], 5);

    let search = serde_json::to_value(example_search_request())?;
    assert_eq!(search["query"], "ReadService");
    assert_eq!(search["scope"], "project");
    Ok(())
}

#[test]
fn minimal_wire_requests_fill_documented_defaults() -> TestResult {
    let get_symbol: GetSymbolParams = serde_json::from_value(json!({"name": "ReadService"}))?;
    assert_eq!(get_symbol, example_get_symbol_request());

    let search: SearchParams = serde_json::from_value(json!({"query": "ReadService"}))?;
    assert_eq!(search, example_search_request());
    Ok(())
}
