//! MCP request durations, in the MCP semantic conventions' spelling.
//!
//! `mcp.server.operation.duration` times every request `RiftMcp` answers, and
//! `mcp.client.operation.duration` every request `rift mcp` forwards to the workspace server.
//! A series names the request's `mcp.method.name`, the `gen_ai.tool.name` of a tool call,
//! and, for a request that did not succeed, `error.type` and `rpc.response.status_code`.
//! A request `RiftMcp` answers also names the session's `mcp.protocol.version` and its
//! `network.transport`. Every label value is a literal: a tool name outside the served
//! tools and a JSON-RPC code outside the codes below leave their label out, and a protocol
//! version rmcp does not know records as `_OTHER`, so a caller cannot grow the series.

use std::time::Duration;

use rift_tracing::Histogram;
use rmcp::ErrorData;
use rmcp::model::{CallToolResponse, ErrorCode, ProtocolVersion};

use crate::failure::RIFT_ERROR_CODE;

/// The `mcp.method.name` of the session's opening request.
pub(crate) const INITIALIZE: &str = "initialize";
/// The `mcp.method.name` of a liveness request.
pub(crate) const PING: &str = "ping";
/// The `mcp.method.name` of a tool call.
pub(crate) const TOOLS_CALL: &str = "tools/call";
/// The `mcp.method.name` of a tool listing.
pub(crate) const TOOLS_LIST: &str = "tools/list";
/// The `mcp.method.name` of a resource listing.
pub(crate) const RESOURCES_LIST: &str = "resources/list";
/// The `mcp.method.name` of a resource template listing.
pub(crate) const RESOURCE_TEMPLATES_LIST: &str = "resources/templates/list";
/// The `mcp.method.name` of a resource read.
pub(crate) const RESOURCES_READ: &str = "resources/read";

/// The tools `RiftMcp` serves, the only values `gen_ai.tool.name` takes.
pub(crate) const SERVED_TOOLS: [&str; 3] = ["get_symbol", "nodes", "search"];

/// The `error.type` of a tool call answered with `isError`, as the conventions spell it.
const TOOL_ERROR: &str = "tool_error";
/// The value of a label whose source is outside the closed set the series names: the
/// `error.type` of a JSON-RPC error whose code is none of [`STATUS_CODES`], and the
/// `mcp.protocol.version` of a version rmcp does not know.
const OTHER: &str = "_OTHER";

/// The `network.transport` of a request `RiftMcp` answers: `rift server` serves it over
/// streamable HTTP on a TCP listener, and the conventions name HTTP `tcp`.
const TRANSPORT_TCP: &str = "tcp";

/// The JSON-RPC error codes a series names, as text: the JSON-RPC 2.0 codes, the MCP codes
/// rmcp declares, and the code Rift carries an operating failure under.
const STATUS_CODES: [(ErrorCode, &str); 10] = [
    (ErrorCode::PARSE_ERROR, "-32700"),
    (ErrorCode::INVALID_REQUEST, "-32600"),
    (ErrorCode::METHOD_NOT_FOUND, "-32601"),
    (ErrorCode::INVALID_PARAMS, "-32602"),
    (ErrorCode::INTERNAL_ERROR, "-32603"),
    (ErrorCode::UNSUPPORTED_PROTOCOL_VERSION, "-32022"),
    (ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY, "-32021"),
    (ErrorCode::HEADER_MISMATCH, "-32020"),
    (ErrorCode::RESOURCE_NOT_FOUND, "-32002"),
    (RIFT_ERROR_CODE, "-32000"),
];

/// The bucket boundaries, in seconds, the MCP conventions advise for both duration metrics.
const MCP_DURATION_BOUNDARIES_SECONDS: [f64; 14] = [
    0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

/// `mcp.server.operation.duration`: one request `RiftMcp` answered, from the handler's
/// start to its answer.
pub(crate) const MCP_SERVER_OPERATION_DURATION: Histogram<6> = Histogram::declare(
    "mcp.server.operation.duration",
    &[
        "mcp.method.name",
        "gen_ai.tool.name",
        "error.type",
        "rpc.response.status_code",
        "mcp.protocol.version",
        "network.transport",
    ],
)
.boundaries(&MCP_DURATION_BOUNDARIES_SECONDS);

/// `mcp.client.operation.duration`: one request `rift mcp` forwarded, from the downstream
/// handler's start to the answer it relays, a reconnect included.
pub(crate) const MCP_CLIENT_OPERATION_DURATION: Histogram<6> = Histogram::declare(
    "mcp.client.operation.duration",
    &[
        "mcp.method.name",
        "gen_ai.tool.name",
        "error.type",
        "rpc.response.status_code",
        "mcp.protocol.version",
        "network.transport",
    ],
)
.boundaries(&MCP_DURATION_BOUNDARIES_SECONDS);

/// One MCP request as its duration series names it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct McpRequest {
    method: &'static str,
    tool: &'static str,
    version: &'static str,
    transport: &'static str,
}

impl McpRequest {
    /// A request of `method` that names no tool.
    pub(crate) const fn method(method: &'static str) -> Self {
        Self {
            method,
            tool: "",
            version: "",
            transport: "",
        }
    }

    /// The request as `RiftMcp` answered it in a session of `version`, the version the
    /// request's context reports: it names that version and the TCP transport.
    pub(crate) fn served(self, version: Option<&ProtocolVersion>) -> Self {
        Self {
            version: protocol_version(version),
            transport: TRANSPORT_TCP,
            ..self
        }
    }

    /// A `tools/call` of the tool `name`; a name outside [`SERVED_TOOLS`] names no tool.
    pub(crate) fn tool_call(name: &str) -> Self {
        Self {
            tool: SERVED_TOOLS
                .into_iter()
                .find(|served| *served == name)
                .unwrap_or(""),
            ..Self::method(TOOLS_CALL)
        }
    }

    /// Records `elapsed` into `histogram` under the request and `ending`. A clock that ran
    /// backwards measured nothing, and records nothing.
    pub(crate) fn record(
        self,
        histogram: &Histogram<6>,
        elapsed: Option<Duration>,
        ending: Ending,
    ) {
        let Some(elapsed) = elapsed else {
            return;
        };
        let (error, status) = match ending {
            Ending::Answered => ("", ""),
            Ending::ToolError => (TOOL_ERROR, ""),
            Ending::Refused(code) => STATUS_CODES
                .iter()
                .find(|(known, _)| *known == code)
                .map_or((OTHER, ""), |(_, text)| (*text, *text)),
        };
        histogram
            .labeled([
                self.method,
                self.tool,
                error,
                status,
                self.version,
                self.transport,
            ])
            .record(elapsed);
    }
}

/// The `mcp.protocol.version` of `version`: its text when rmcp knows the version,
/// `_OTHER` for any other, and no value when the session negotiated none.
fn protocol_version(version: Option<&ProtocolVersion>) -> &'static str {
    let Some(version) = version else {
        return "";
    };
    ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .find(|known| *known == version)
        .map_or(OTHER, ProtocolVersion::as_str)
}

/// How one MCP request ended, as its duration series names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ending {
    /// The request was answered with a result.
    Answered,
    /// A tool call was answered with a result whose `isError` is set.
    ToolError,
    /// The request was answered with the JSON-RPC error of this code.
    Refused(ErrorCode),
}

impl Ending {
    /// How a request answered with `result` ended.
    pub(crate) fn of<Value>(result: &Result<Value, ErrorData>) -> Self {
        match result {
            Ok(_) => Self::Answered,
            Err(error) => Self::Refused(error.code),
        }
    }

    /// How a tool call answered with `result` ended: a completed result with `isError` set
    /// is a tool error.
    pub(crate) fn of_tool_call(result: &Result<CallToolResponse, ErrorData>) -> Self {
        match result {
            Ok(CallToolResponse::Complete(result)) if result.is_error == Some(true) => {
                Self::ToolError
            }
            other => Self::of(other),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use rift_tracing::SeriesValue;
    use rmcp::ErrorData;
    use rmcp::model::{CallToolResponse, CallToolResult, ErrorCode, ProtocolVersion};

    use super::{Ending, MCP_SERVER_OPERATION_DURATION, McpRequest, RESOURCES_READ, SERVED_TOOLS};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// The count the series of `name` under `labels` holds, or zero when it holds none.
    pub(crate) fn recorded(
        snapshot: &rift_tracing::MetricSnapshot,
        name: &str,
        labels: &[(&str, &str)],
    ) -> u64 {
        match snapshot
            .find(name, labels)
            .map(rift_tracing::MetricSeries::value)
        {
            Some(SeriesValue::Buckets { count, .. }) => *count,
            _ => 0,
        }
    }

    #[test]
    fn the_served_tools_are_the_tools_the_router_lists() {
        let mut listed: Vec<String> = crate::server::RiftMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        listed.sort();
        assert_eq!(listed, SERVED_TOOLS);
    }

    #[test]
    fn a_tool_error_and_a_refusal_name_their_error_type_and_status_code() -> TestResult {
        let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let elapsed = Some(Duration::from_millis(3));
        let tool_error: Result<CallToolResponse, ErrorData> =
            Ok(CallToolResult::error(Vec::new()).into());
        McpRequest::tool_call("search").record(
            &MCP_SERVER_OPERATION_DURATION,
            elapsed,
            Ending::of_tool_call(&tool_error),
        );
        McpRequest::tool_call("unserved").record(
            &MCP_SERVER_OPERATION_DURATION,
            elapsed,
            Ending::Refused(ErrorCode::INVALID_PARAMS),
        );
        McpRequest::method(RESOURCES_READ).record(
            &MCP_SERVER_OPERATION_DURATION,
            elapsed,
            Ending::Refused(ErrorCode(-32099)),
        );
        McpRequest::method(RESOURCES_READ).record(
            &MCP_SERVER_OPERATION_DURATION,
            None,
            Ending::Answered,
        );
        let snapshot = recorder.metrics();
        let name = "mcp.server.operation.duration";
        assert_eq!(
            recorded(
                &snapshot,
                name,
                &[
                    ("mcp.method.name", "tools/call"),
                    ("gen_ai.tool.name", "search"),
                    ("error.type", "tool_error"),
                ],
            ),
            1
        );
        assert_eq!(
            recorded(
                &snapshot,
                name,
                &[
                    ("mcp.method.name", "tools/call"),
                    ("error.type", "-32602"),
                    ("rpc.response.status_code", "-32602"),
                ],
            ),
            1,
            "an unserved tool name leaves gen_ai.tool.name out"
        );
        assert_eq!(
            recorded(
                &snapshot,
                name,
                &[
                    ("mcp.method.name", "resources/read"),
                    ("error.type", "_OTHER")
                ],
            ),
            1,
            "an undeclared code names no status code, and a clock regression records nothing"
        );
        Ok(())
    }

    #[test]
    fn a_served_request_names_a_known_protocol_version_and_others_as_other() -> TestResult {
        let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let elapsed = Some(Duration::from_millis(3));
        let unknown: ProtocolVersion = serde_json::from_str("\"1999-01-01\"")?;
        for version in [Some(&ProtocolVersion::V_2025_06_18), Some(&unknown), None] {
            McpRequest::method(RESOURCES_READ).served(version).record(
                &MCP_SERVER_OPERATION_DURATION,
                elapsed,
                Ending::Answered,
            );
        }
        let snapshot = recorder.metrics();
        let name = "mcp.server.operation.duration";
        for version in ["2025-06-18", "_OTHER"] {
            assert_eq!(
                recorded(
                    &snapshot,
                    name,
                    &[
                        ("mcp.method.name", "resources/read"),
                        ("mcp.protocol.version", version),
                        ("network.transport", "tcp"),
                    ],
                ),
                1,
                "{version}: {snapshot:?}"
            );
        }
        assert_eq!(
            recorded(
                &snapshot,
                name,
                &[
                    ("mcp.method.name", "resources/read"),
                    ("network.transport", "tcp"),
                ],
            ),
            1,
            "a session without a version leaves mcp.protocol.version out"
        );
        Ok(())
    }
}
