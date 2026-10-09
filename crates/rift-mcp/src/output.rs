//! Tool answer output: compact text, structured content, and proxy selection.

mod failure;
mod policy;
mod render;
mod text;

use rift_error::{RiftError, errors};
use rift_protocol::error as wire;
use rmcp::ErrorData;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock};
use serde::Serialize;

use crate::failure::{McpErrorExt as _, WireFailure as _};
use render::{Render, text_of};
use text::TextError;

pub(crate) use failure::ToolFailure;
pub use policy::OutputPolicy;

/// A typed tool answer that serves as compact text and as structured content.
///
/// The name is `Json` because the rmcp tool macro derives `outputSchema` from a return type whose
/// final path identifier is `Json`, and the schema comes from the wrapped type.
pub(crate) struct Json<T>(pub(crate) T);

impl<T: Render + Serialize> IntoCallToolResult for Json<T> {
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        let text = match text_of(&self.0) {
            Ok(text) => text,
            Err(error) => return refused(AnswerFault::Text(error)),
        };
        let structured = match serde_json::to_value(&self.0) {
            Ok(structured) => structured,
            Err(error) => return refused(AnswerFault::Structured(error)),
        };
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        result.structured_content = Some(structured);
        Ok(result.into())
    }
}

/// The compact text of a resource, or the JSON-RPC error a resource read answers with.
///
/// A resource read has no `isError` form, so a text the writer refuses is the same registered
/// failure a tool answer reports, carried as the error object of the read.
///
/// # Errors
///
/// Returns the registered failure when the text crosses the byte limit or cannot be written.
pub(crate) fn resource_text<T: Render + ?Sized>(answer: &T) -> Result<String, ErrorData> {
    text_of(answer).map_err(|error| {
        answer_error(AnswerFault::Text(error))
            .mcp()
            .tool_error(wire::ErrorPhase::Read)
    })
}

/// Why an answer could not be served.
enum AnswerFault {
    /// The text writer refused the answer.
    Text(TextError),
    /// The answer did not serialize into structured content.
    Structured(serde_json::Error),
}

/// The completed error result for an answer that could not be served.
fn refused(fault: AnswerFault) -> Result<CallToolResponse, ErrorData> {
    answer_error(fault)
        .mcp()
        .tool_failure(wire::ErrorPhase::Read)
        .into_call_tool_result()
}

/// The registered error for an answer that could not be served.
///
/// Crossing the text limit is `answer_text_limit`; every other fault keeps its error as source.
fn answer_error(fault: AnswerFault) -> RiftError {
    match fault {
        AnswerFault::Text(TextError::Overflow(overflow)) => errors::mcp::answer_text_limit()
            .limit(overflow.limit)
            .error(),
        AnswerFault::Text(error) => errors::mcp::answer_text_failed().source(error).error(),
        AnswerFault::Structured(error) => {
            errors::mcp::answer_structure_failed().source(error).error()
        }
    }
}

#[cfg(test)]
#[path = "output/json_tests.rs"]
mod tests;
