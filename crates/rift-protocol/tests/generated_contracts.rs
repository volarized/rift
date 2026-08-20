//! Generated contract metadata tests.

use rift_protocol::generated::contracts::TOOL_CONTRACTS;

#[test]
fn contracts_expose_only_implemented_read_tools() {
    let tools = TOOL_CONTRACTS
        .iter()
        .map(|contract| {
            (
                contract.name,
                contract.request_model,
                contract.result_model,
                contract.minimal_request_json,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        tools,
        [
            (
                "get_symbol",
                "GetSymbolParams",
                "GetSymbolResult",
                r#"{"name":"BaseModel"}"#,
            ),
            (
                "search",
                "SearchParams",
                "SearchResult",
                r#"{"query":"BaseModel"}"#,
            ),
        ]
    );
}
