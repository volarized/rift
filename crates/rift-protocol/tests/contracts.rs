//! Tool contract metadata tests.

use std::error::Error;

use rift_protocol::contracts::TOOL_CONTRACTS;
use rift_protocol::read::{GetSymbolParams, SearchParams};

type TestResult = Result<(), Box<dyn Error>>;

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
fn minimal_requests_parse_into_request_models() -> TestResult {
    for contract in TOOL_CONTRACTS {
        match contract.name {
            "get_symbol" => {
                serde_json::from_str::<GetSymbolParams>(contract.minimal_request_json)?;
            }
            "search" => {
                serde_json::from_str::<SearchParams>(contract.minimal_request_json)?;
            }
            unknown => return Err(format!("contract for unmapped tool {unknown}").into()),
        }
    }
    Ok(())
}
