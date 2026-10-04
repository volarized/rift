//! Tool failures: the one boundary from a failed tool path to the MCP result.
//!
//! A Rift operating failure is a completed result with `isError` set and its text in one block.
//! Every other failure stays the JSON-RPC error the tool path returned.

use rift_protocol::error as wire;
use rmcp::ErrorData;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock};
use serde::Deserialize as _;

use super::render::text_of;
use crate::failure::RIFT_ERROR_CODE;

/// The error a tool call ended in, wrapping the rmcp error the router returned.
pub(crate) struct ToolFailure(ErrorData);

impl ToolFailure {
    /// The typed wire error, when the wrapped error is a Rift operating failure.
    ///
    /// That is the Rift code with `data` that decodes into the typed wire error.
    fn operating_failure(&self) -> Option<wire::ErrorData> {
        if self.0.code != RIFT_ERROR_CODE {
            return None;
        }
        wire::ErrorData::deserialize(self.0.data.as_ref()?).ok()
    }
}

impl From<ErrorData> for ToolFailure {
    fn from(error: ErrorData) -> Self {
        Self(error)
    }
}

impl IntoCallToolResult for ToolFailure {
    /// An operating failure completes with `isError` and its text. Any other failure, or one
    /// whose text cannot be written, stays the JSON-RPC error.
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        match self.operating_failure().map(|error| text_of(&error)) {
            Some(Ok(text)) => Ok(CallToolResult::error(vec![ContentBlock::text(text)]).into()),
            Some(Err(_)) | None => Err(self.0),
        }
    }
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod tests;
