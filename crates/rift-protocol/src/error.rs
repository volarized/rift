//! Wire models for Rift MCP error responses.
//!
//! Every type here is a wire contract: serde attributes define exactly what
//! the server accepts and returns, and the MCP server derives its advertised
//! request and response schemas from these definitions.

use crate::read::DiagnosticContext;
use crate::schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Stable failure class for one request. `ErrorData.retry` carries the
/// instance-specific retry decision; unsupported coverage and edit refusal
/// use typed domain results instead.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum ErrorCode {
    /// The request does not satisfy the schema, or names something the schema forbids: a
    /// `limit` above the advertised maximum, a filter field that does not exist.
    #[serde(rename = "invalid_request")]
    InvalidRequest,
    /// The caller cannot perform this operation: a path addresses through a symlink
    /// component, which following could take outside the workspace.
    #[serde(rename = "permission_denied")]
    PermissionDenied,
    /// The identity is well-formed and resolves to nothing, such as a missing symbol or
    /// filesystem entry.
    #[serde(rename = "resource_not_found")]
    ResourceNotFound,
    /// The entry is known but its bytes cannot be read.
    #[serde(rename = "content_unavailable")]
    ContentUnavailable,
    /// The cursor is malformed, or it was minted for a different request, order or page
    /// size.
    #[serde(rename = "cursor_invalid")]
    CursorInvalid,
    /// The cursor is valid, but its captured result page set left the process-local cache
    /// through eviction or process restart.
    #[serde(rename = "cursor_expired")]
    CursorExpired,
    /// The caller cancelled, or the connection closed before the request finished.
    #[serde(rename = "cancelled")]
    Cancelled,
    /// A request, response, or capture crossed an advertised limit. `limit` identifies the
    /// bound and required value.
    #[serde(rename = "limit_exceeded")]
    LimitExceeded,
    /// Rift could not read or write workspace files.
    #[serde(rename = "storage_failure")]
    StorageFailure,
    /// A violated Rift invariant. `causes` identifies the operation and concrete failure.
    #[serde(rename = "internal_error")]
    InternalError,
    /// The workspace contains a path the protocol cannot represent safely.
    #[serde(rename = "unsupported_path")]
    UnsupportedPath,
    /// The resource exists but cannot serve this request now, such as a fresh indexed read
    /// that could not capture stable revisions. The instance's `retry` field says whether
    /// the same request is worth sending again.
    #[serde(rename = "temporarily_unavailable")]
    TemporarilyUnavailable,
    /// The workspace configuration does not satisfy its schema. New requests remain
    /// blocked until it is valid.
    #[serde(rename = "configuration_invalid")]
    ConfigurationInvalid,
    /// The tool exists, but workspace configuration and the served providers cannot answer
    /// this operation for the requested language.
    #[serde(rename = "capability_unavailable")]
    CapabilityUnavailable,
}

/// Stable retry instruction for one failed request. `temporarily_unavailable` can permit
/// the same request; `invalid_request` requires changed input.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum RetryDirective {
    /// The request fails the same way every time. Change it before sending it again.
    #[serde(rename = "never")]
    Never,
    /// Send the same bytes again. The cause was transient, such as an index still filling.
    #[serde(rename = "same_request")]
    SameRequest,
    /// Resolve the condition with a local state command or configuration change, then send
    /// the request again.
    #[serde(rename = "operator_action")]
    OperatorAction,
}

/// How far the request got before it failed. The same code means different things at
/// different phases: `limit_exceeded` while reading is a response too big, and while
/// checking a change it is a change set too large.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum ErrorPhase {
    /// Reading workspace or provider data.
    #[serde(rename = "read")]
    Read,
    /// Turning an address or a cursor into the concrete thing it names at a state.
    #[serde(rename = "resolve")]
    Resolve,
    /// Checking a proposed change against the state it was pinned to.
    #[serde(rename = "check")]
    Check,
    /// Resolving the operation and writing the result into the targeted tree.
    #[serde(rename = "change")]
    Change,
}

/// One cause in a failure chain. Entries appear from the outer operation to the concrete
/// failure.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorCause {
    /// How this link classifies on its own.
    pub code: ErrorCode,
    /// What happened, for a human reading a log.
    #[schemars(length(max = 4096))]
    pub message: String,
    /// What could be done about this cause. The request's outer directive governs the
    /// call.
    pub retry: RetryDirective,
}

/// Which advertised limit a `limit_exceeded` failure hit, and by how much. It is present
/// exactly when the code is `limit_exceeded`; without it, choosing between retrying
/// smaller, falling back to another resource and giving up means reparsing the human
/// message.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LimitEvidence {
    /// The limit's field path, such as `max_page_items`.
    #[schemars(length(min = 1, max = 128))]
    pub field: String,
    /// The value in force when the request was rejected.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub limit: u64,
    /// What the request would have needed. Larger than `limit`, and the difference is what
    /// the caller has to close.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub required: u64,
}

/// The `data` object on every Rift MCP failure. `code` and `retry` are what a caller
/// branches on, `message` is for a human, and the remaining fields carry the evidence
/// available for that failure.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::error_limit_rides_limit_exceeded)]
pub struct ErrorData {
    /// Why the request failed.
    pub code: ErrorCode,
    /// Human-readable account of the failure. Machine-readable classification remains in
    /// the surrounding fields.
    #[schemars(length(max = 4096))]
    pub message: String,
    /// What the caller may do next.
    pub retry: RetryDirective,
    /// How far the request got before it failed.
    pub phase: ErrorPhase,
    /// What a provider reported while the request was failing. Empty where none ran.
    #[schemars(length(max = 64))]
    pub diagnostics: Vec<DiagnosticContext>,
    /// Which advertised limit was hit, and by how much. Present exactly when `code` is
    /// `limit_exceeded`, and forbidden otherwise.
    #[serde(default)]
    pub limit: Option<LimitEvidence>,
    /// What led to this failure, outermost first. A code alone rarely says whether the
    /// cause is worth waiting out.
    #[schemars(length(max = 8))]
    pub causes: Vec<ErrorCause>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::schema_for;
    use serde_json::json;

    #[test]
    fn error_data_round_trips_through_json() {
        let data = ErrorData {
            code: ErrorCode::LimitExceeded,
            message: "response exceeded max_page_items".to_owned(),
            retry: RetryDirective::SameRequest,
            phase: ErrorPhase::Read,
            diagnostics: Vec::new(),
            limit: Some(LimitEvidence {
                field: "max_page_items".to_owned(),
                limit: 100,
                required: 250,
            }),
            causes: vec![ErrorCause {
                code: ErrorCode::LimitExceeded,
                message: "search page exceeded max_page_items".to_owned(),
                retry: RetryDirective::SameRequest,
            }],
        };
        let value = serde_json::to_value(&data).expect("serialize");
        let round_tripped: ErrorData = serde_json::from_value(value).expect("deserialize");
        assert_eq!(round_tripped, data);
    }

    #[test]
    fn error_data_without_limit_key_deserializes_to_none() {
        let data: ErrorData = serde_json::from_value(json!({
            "code": "internal_error",
            "message": "invariant violated",
            "retry": "never",
            "phase": "read",
            "diagnostics": [],
            "causes": []
        }))
        .expect("deserialize");
        assert_eq!(data.limit, None);
    }

    #[test]
    fn error_data_rejects_unknown_field() {
        let result = serde_json::from_value::<ErrorData>(json!({
            "code": "internal_error",
            "message": "invariant violated",
            "retry": "never",
            "phase": "read",
            "diagnostics": [],
            "causes": [],
            "protocol": null
        }));
        assert!(result.is_err());
    }

    #[test]
    fn error_data_schema_carries_limit_exceeded_if_then_else() {
        let schema = serde_json::to_value(schema_for!(ErrorData)).expect("schema");
        let clauses = schema["allOf"].as_array().expect("allOf array");
        let clause = clauses
            .iter()
            .find(|clause| clause["if"]["properties"]["code"]["const"] == json!("limit_exceeded"))
            .expect("limit_exceeded if/then/else clause");
        assert_eq!(clause["if"]["required"], json!(["code"]));
        assert_eq!(clause["then"]["required"], json!(["limit"]));
        assert_eq!(clause["else"]["not"]["required"], json!(["limit"]));
    }

    #[test]
    fn error_code_serializes_to_exact_wire_string() {
        let cases = [
            (ErrorCode::InvalidRequest, "invalid_request"),
            (ErrorCode::PermissionDenied, "permission_denied"),
            (ErrorCode::ResourceNotFound, "resource_not_found"),
            (ErrorCode::ContentUnavailable, "content_unavailable"),
            (ErrorCode::CursorInvalid, "cursor_invalid"),
            (ErrorCode::CursorExpired, "cursor_expired"),
            (ErrorCode::Cancelled, "cancelled"),
            (ErrorCode::LimitExceeded, "limit_exceeded"),
            (ErrorCode::StorageFailure, "storage_failure"),
            (ErrorCode::InternalError, "internal_error"),
            (ErrorCode::UnsupportedPath, "unsupported_path"),
            (ErrorCode::TemporarilyUnavailable, "temporarily_unavailable"),
            (ErrorCode::ConfigurationInvalid, "configuration_invalid"),
            (ErrorCode::CapabilityUnavailable, "capability_unavailable"),
        ];
        for (code, wire) in cases {
            assert_eq!(serde_json::to_value(code).expect("serialize"), json!(wire));
        }
    }
}
