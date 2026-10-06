//! Tool failures: the one boundary from a failed tool path to the MCP result.
//!
//! A Rift operating failure is a completed result with `isError` set and its text in one block.
//! A registered failure a tool path raised keeps its [`RiftError`](rift_error::RiftError) up to
//! that text, which writes its registered identity. Every other failure stays the JSON-RPC error
//! the tool path returned.

use std::fmt;

use rift_protocol::error as wire;
use rmcp::ErrorData;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock};
use serde::Deserialize as _;

use super::render::{RegisteredFailure, text_of};
use super::text::TextError;
use crate::failure::{McpFailure, RIFT_ERROR_CODE, WireFailure as _};

/// The error a tool call ended in.
#[derive(Debug)]
pub(crate) enum ToolFailure {
    /// A registered failure a tool path raised, and the phase it stopped in.
    Registered(McpFailure, wire::ErrorPhase),
    /// The rmcp error the router returned, such as a refused parameter or an unknown tool.
    Router(ErrorData),
}

impl ToolFailure {
    /// The typed wire error, when the router error is a Rift operating failure.
    ///
    /// That is the Rift code with `data` that decodes into the typed wire error.
    fn operating_failure(error: &ErrorData) -> Option<wire::ErrorData> {
        if error.code != RIFT_ERROR_CODE {
            return None;
        }
        wire::ErrorData::deserialize(error.data.as_ref()?).ok()
    }
}

/// The text of a registered failure: the typed wire error of `failure` stopped in `phase`, with
/// its registered identity.
///
/// The message is the `Display` form of the registered error: its message with its evidence, then
/// its action. A failure whose registry entry defines no action writes the message alone.
fn registered_text(failure: &McpFailure, phase: wire::ErrorPhase) -> Result<String, TextError> {
    let error = failure.registered();
    let mut wire = failure.wire_error(phase);
    if error.action().is_empty() {
        wire.message = error.detail();
    }
    text_of(&RegisteredFailure {
        identity: error.slug().as_str(),
        error: &wire,
    })
}

impl fmt::Display for ToolFailure {
    /// The registered error, or the router error as rmcp writes it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registered(failure, _) => fmt::Display::fmt(failure, formatter),
            Self::Router(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for ToolFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registered(failure, _) => std::error::Error::source(failure),
            Self::Router(_) => None,
        }
    }
}

impl From<ErrorData> for ToolFailure {
    fn from(error: ErrorData) -> Self {
        Self::Router(error)
    }
}

impl From<ToolFailure> for ErrorData {
    /// The JSON-RPC error object of the failure, for a caller that answers with one, such as a
    /// resource read.
    fn from(failure: ToolFailure) -> Self {
        match failure {
            ToolFailure::Registered(failure, phase) => failure.tool_error(phase),
            ToolFailure::Router(error) => error,
        }
    }
}

impl IntoCallToolResult for ToolFailure {
    /// An operating failure completes with `isError` and its text. Any other failure, or one
    /// whose text cannot be written, stays the JSON-RPC error.
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        let text = match &self {
            Self::Registered(failure, phase) => Some(registered_text(failure, *phase)),
            Self::Router(error) => Self::operating_failure(error).map(|error| text_of(&error)),
        };
        match text {
            Some(Ok(text)) => Ok(CallToolResult::error(vec![ContentBlock::text(text)]).into()),
            Some(Err(_)) | None => Err(self.into()),
        }
    }
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod tests;
