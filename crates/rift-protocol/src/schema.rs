//! Deterministic JSON Schema export of the protocol surface.
//!
//! The Rust models in [`crate::read`] are the source of truth; this module
//! renders them into the single JSON document the docs site publishes. The
//! document is deterministic because `serde_json` stores objects as sorted
//! maps, so equal models always render equal bytes.

use schemars::generate::{SchemaGenerator, SchemaSettings};
use serde_json::{Value, json};

use crate::contracts::TOOL_CONTRACTS;
use crate::read::{
    GetSymbolParams, GetSymbolResult, NodesParams, NodesResult, SearchParams, SearchResult,
};

/// JSON Schema draft the exported document and every definition target.
const SCHEMA_DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Path under which shared definitions are collected and referenced.
const DEFINITIONS_PATH: &str = "#/$defs";

/// One-line summary rendered at the top of the exported document.
const DOCUMENT_DESCRIPTION: &str = "JSON values accepted and returned by the Rift MCP server.";

/// Renders the protocol schema document the docs site publishes.
///
/// The document carries the schema draft, a short description, one entry per
/// implemented MCP tool with `$ref`s into `$defs`, and the definitions of all
/// tool request and result models plus their transitive dependencies. Output
/// is pretty-printed, ends with a trailing newline, and is byte-identical
/// across calls.
#[must_use]
pub fn schema_document() -> String {
    let settings = SchemaSettings::draft2020_12()
        .with(|settings| settings.definitions_path = format!("{DEFINITIONS_PATH}/").into());
    let mut generator = SchemaGenerator::new(settings);
    generator.subschema_for::<GetSymbolParams>();
    generator.subschema_for::<GetSymbolResult>();
    generator.subschema_for::<SearchParams>();
    generator.subschema_for::<SearchResult>();
    generator.subschema_for::<NodesParams>();
    generator.subschema_for::<NodesResult>();
    let definitions: Value = generator.take_definitions(true).into_iter().collect();

    let tools: Vec<Value> = TOOL_CONTRACTS
        .iter()
        .map(|contract| {
            json!({
                "name": contract.name,
                "description": contract.description,
                "input": { "$ref": format!("{DEFINITIONS_PATH}/{}", contract.request_model) },
                "output": { "$ref": format!("{DEFINITIONS_PATH}/{}", contract.result_model) },
            })
        })
        .collect();

    let document = json!({
        "$schema": SCHEMA_DRAFT,
        "description": DOCUMENT_DESCRIPTION,
        "tools": tools,
        "$defs": definitions,
    });
    let mut rendered = format!("{document:#}");
    rendered.push('\n');
    rendered
}
