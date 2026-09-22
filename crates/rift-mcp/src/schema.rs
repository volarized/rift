//! Deterministic export of the served MCP tool surface.
//!
//! [`schema_document`] serializes the same tool router [`RiftMcp`] runs, so
//! the exported document and the served surface come from one definition:
//! tool names and descriptions from the `#[tool]` methods, request and
//! response schemas derived from the `rift_protocol` wire models.
//! Export orchestration lives in `rift-schema-export`; this module owns only
//! the schema for the MCP surface served by [`RiftMcp`].

use serde_json::json;

use crate::RiftMcp;

/// One-line summary rendered at the top of the exported document.
const DOCUMENT_DESCRIPTION: &str =
    "Tools served by the Rift MCP server, with the JSON Schemas derived from the Rust wire models.";

/// Renders the document the docs site publishes: one entry per served tool
/// with its name, description, input schema, and output schema, sorted by
/// name.
///
/// Output is pretty-printed, ends with a trailing newline, and is
/// byte-identical across calls because `serde_json` stores objects as sorted
/// maps.
#[must_use]
pub fn schema_document() -> String {
    let mut tools = RiftMcp::tool_router().list_all();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let tools: Vec<_> = tools
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
                "output_schema": tool.output_schema,
            })
        })
        .collect();
    let document = json!({
        "description": DOCUMENT_DESCRIPTION,
        "tools": tools,
    });
    rift_protocol::schema::render_json_document(document)
}

/// Returns the served tool listing `RiftMcp` runs, sorted by name.
///
/// `RiftMcp::tool_router` is `pub(crate)`, so this is the one path a
/// consumer outside this crate has to the same [`rmcp::model::Tool`] values
/// [`schema_document`] serializes. `rift install claude` reads each tool's
/// name, description, and JSON Schema straight from these typed values to
/// generate its Claude Code skill, rather than round-tripping the exported
/// document back through JSON.
#[must_use]
pub fn tool_listing() -> Vec<rmcp::model::Tool> {
    RiftMcp::tool_router().list_all()
}
