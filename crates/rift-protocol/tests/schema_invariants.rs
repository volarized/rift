//! Invariants every exported schema document must hold.

use std::error::Error;

use rift_protocol::contracts::TOOL_CONTRACTS;
use rift_protocol::schema::schema_document;
use serde_json::{Map, Value};

type TestResult = Result<(), Box<dyn Error>>;

fn exported_definitions() -> Result<Map<String, Value>, Box<dyn Error>> {
    let document: Value = serde_json::from_str(&schema_document())?;
    document
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| "exported document must hold an object $defs".into())
}

#[test]
fn every_definition_carries_a_description() -> TestResult {
    let definitions = exported_definitions()?;
    assert!(!definitions.is_empty(), "$defs must not be empty");
    for (name, definition) in &definitions {
        let description = definition
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            !description.trim().is_empty(),
            "definition {name} must carry a non-empty description"
        );
    }
    Ok(())
}

#[test]
fn every_contract_model_is_defined() -> TestResult {
    let definitions = exported_definitions()?;
    for contract in TOOL_CONTRACTS {
        assert!(
            definitions.contains_key(contract.request_model),
            "$defs must define request model {} for tool {}",
            contract.request_model,
            contract.name
        );
        assert!(
            definitions.contains_key(contract.result_model),
            "$defs must define result model {} for tool {}",
            contract.result_model,
            contract.name
        );
    }
    Ok(())
}

#[test]
fn retired_vocabulary_is_absent() {
    let document = schema_document();
    assert!(
        !document.to_lowercase().contains(concat!("proven", "ance")),
        "exported document must use the origins vocabulary everywhere"
    );
}

#[test]
fn tools_are_exactly_the_implemented_read_surface() -> TestResult {
    let document: Value = serde_json::from_str(&schema_document())?;
    let names = document
        .get("tools")
        .and_then(Value::as_array)
        .ok_or("exported document must hold a tools array")?
        .iter()
        .map(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or("every tool entry must carry a string name")
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(names, ["get_symbol", "search"]);
    Ok(())
}
