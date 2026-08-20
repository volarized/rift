//! Rust JSON Schema generator tests.

#[path = "../schema_codegen.rs"]
mod schema_codegen;

use std::error::Error;

use rift_protocol::MCP_SCHEMA;

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn canonical_schema_generation_is_deterministic() -> TestResult {
    let first = schema_codegen::generate(MCP_SCHEMA)?;
    let second = schema_codegen::generate(MCP_SCHEMA)?;

    assert_eq!(first.read, second.read);
    assert_eq!(first.contracts, second.contracts);
    Ok(())
}

#[test]
fn missing_definitions_are_rejected() {
    let error = schema_codegen::generate(r#"{"rift:entryPoints":{"mcp.tools":{}}}"#)
        .expect_err("schema without definitions must fail");

    assert_eq!(error.to_string(), "canonical schema has no object $defs");
}
