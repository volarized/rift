//! JSON-RPC correlation: id allocation, pending matching, and envelopes.
//!
//! The session allocates one numeric id per request and matches every
//! incoming payload back to what it answers. Envelope construction and
//! classification live here so the byte layer in `framing` never inspects
//! JSON.

use std::collections::BTreeMap;

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, LimitEvidence, fault_label};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum requests awaiting a response at once. The session is
/// request-scoped and sequential; pending entries above one exist only for
/// requests a caller cancelled mid-read.
pub const PENDING_REQUESTS_MAX: usize = 32;

/// The JSON-RPC protocol version every envelope carries.
const JSON_RPC_VERSION: &str = "2.0";

/// The JSON-RPC error code answering a method this client does not serve.
pub const METHOD_NOT_FOUND_CODE: i64 = -32601;

/// One allocated request id.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RequestId(u64);

impl RequestId {
    /// The numeric id as allocated.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// How correlation broke down.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrelationFault {
    /// A new request would cross [`PENDING_REQUESTS_MAX`].
    PendingRequestsExceeded,
    /// The engine answered an id no pending request carries.
    ResponseUnknown {
        /// The id as received.
        id: String,
    },
}

impl Fault for CorrelationFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::PendingRequestsExceeded => ErrorName::Wire(ErrorCode::LimitExceeded),
            Self::ResponseUnknown { .. } => ErrorName::Wire(ErrorCode::CapabilityUnavailable),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("fault", fault_label(self))];
        if let Self::ResponseUnknown { id } = self {
            context.push(ErrorContext::new("id", id.clone()));
        }
        context
    }

    fn limit_evidence(&self) -> Option<LimitEvidence> {
        match self {
            Self::PendingRequestsExceeded => Some(LimitEvidence {
                field: "correlation.pending_requests_max".to_owned(),
                limit: u64::try_from(PENDING_REQUESTS_MAX).unwrap_or(u64::MAX),
                required: u64::try_from(PENDING_REQUESTS_MAX).unwrap_or(u64::MAX) + 1,
            }),
            Self::ResponseUnknown { .. } => None,
        }
    }
}

/// A broken correlation between requests and responses.
pub type CorrelationError = Error<CorrelationFault>;

/// Allocates request ids and matches responses back to their methods.
#[derive(Debug, Default)]
pub struct Correlation {
    next_id: u64,
    pending: BTreeMap<u64, &'static str>,
}

impl Correlation {
    /// An empty correlation with no pending requests.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates the id for one outgoing request and marks it pending.
    ///
    /// # Errors
    ///
    /// Returns [`CorrelationError`] when [`PENDING_REQUESTS_MAX`] requests
    /// already await answers.
    pub fn begin(&mut self, method: &'static str) -> Result<RequestId, CorrelationError> {
        if self.pending.len() >= PENDING_REQUESTS_MAX {
            return Err(Error::new(CorrelationFault::PendingRequestsExceeded));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, method);
        Ok(RequestId(id))
    }

    /// Settles one response id, returning the method it answered.
    ///
    /// # Errors
    ///
    /// Returns [`CorrelationError`] when the id matches no pending request,
    /// including any non-numeric id, which this client never allocates.
    pub fn conclude(&mut self, id: &Value) -> Result<&'static str, CorrelationError> {
        let unknown = || Error::new(CorrelationFault::ResponseUnknown { id: id.to_string() });
        let id = id.as_u64().ok_or_else(unknown)?;
        self.pending.remove(&id).ok_or_else(unknown)
    }
}

/// One incoming JSON-RPC payload, classified by which fields it carries.
#[derive(Debug, Deserialize)]
pub struct Incoming {
    /// The request or response id; absent on notifications.
    #[serde(default)]
    pub id: Option<Value>,
    /// The method; absent on responses.
    #[serde(default)]
    pub method: Option<String>,
    /// Request or notification parameters.
    #[serde(default)]
    pub params: Option<Value>,
    /// A response's answer.
    #[serde(default)]
    pub result: Option<Value>,
    /// A response's failure.
    #[serde(default)]
    pub error: Option<ResponseError>,
}

/// The error object of one failed JSON-RPC response.
#[derive(Debug, Deserialize)]
pub struct ResponseError {
    /// The JSON-RPC error code.
    pub code: i64,
    /// What the engine reported.
    pub message: String,
}

/// Classifies one decoded payload, or nothing when it fits no envelope.
#[must_use]
pub fn classify(payload: &[u8]) -> Option<Incoming> {
    let incoming: Incoming = serde_json::from_slice(payload).ok()?;
    match (&incoming.method, &incoming.id) {
        (None, None) => None,
        _ => Some(incoming),
    }
}

/// Serializes one outgoing request envelope.
#[must_use]
pub fn request(id: RequestId, method: &str, params: &Value) -> Vec<u8> {
    envelope(&serde_json::json!({
        "jsonrpc": JSON_RPC_VERSION,
        "id": id,
        "method": method,
        "params": params,
    }))
}

/// Serializes one outgoing notification envelope.
#[must_use]
pub fn notification(method: &str, params: &Value) -> Vec<u8> {
    envelope(&serde_json::json!({
        "jsonrpc": JSON_RPC_VERSION,
        "method": method,
        "params": params,
    }))
}

/// Serializes one successful response to a server-initiated request.
#[must_use]
pub fn response(id: &Value, result: &Value) -> Vec<u8> {
    envelope(&serde_json::json!({
        "jsonrpc": JSON_RPC_VERSION,
        "id": id,
        "result": result,
    }))
}

/// Serializes one error response to a server-initiated request.
#[must_use]
pub fn error_response(id: &Value, code: i64, message: &str) -> Vec<u8> {
    envelope(&serde_json::json!({
        "jsonrpc": JSON_RPC_VERSION,
        "id": id,
        "error": { "code": code, "message": message },
    }))
}

/// One envelope's bytes; `serde_json::Value` trees always serialize.
fn envelope(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_allocates_increasing_ids_and_conclude_returns_the_method() {
        let mut correlation = Correlation::new();
        let first = correlation.begin("initialize").expect("first id");
        let second = correlation.begin("textDocument/rename").expect("second id");
        assert_ne!(first, second);
        assert_eq!(
            correlation.conclude(&serde_json::json!(1)).expect("method"),
            "textDocument/rename"
        );
        assert_eq!(
            correlation.conclude(&serde_json::json!(0)).expect("method"),
            "initialize"
        );
    }

    #[test]
    fn pending_requests_are_bounded_with_limit_evidence() {
        let mut correlation = Correlation::new();
        for _ in 0..PENDING_REQUESTS_MAX {
            correlation.begin("shutdown").expect("below the bound");
        }
        let error = correlation
            .begin("shutdown")
            .expect_err("the bound must refuse");
        assert_eq!(*error.fault(), CorrelationFault::PendingRequestsExceeded);
        assert!(error.fault().limit_evidence().is_some());
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::LimitExceeded));
    }

    #[test]
    fn unknown_and_non_numeric_response_ids_are_refused() {
        let mut correlation = Correlation::new();
        let unknown = correlation
            .conclude(&serde_json::json!(7))
            .expect_err("never allocated");
        assert_eq!(
            *unknown.fault(),
            CorrelationFault::ResponseUnknown { id: "7".to_owned() }
        );
        let textual = correlation
            .conclude(&serde_json::json!("seven"))
            .expect_err("string ids are never allocated");
        assert!(matches!(
            textual.fault(),
            CorrelationFault::ResponseUnknown { .. }
        ));
        assert_eq!(
            textual.name(),
            ErrorName::Wire(ErrorCode::CapabilityUnavailable)
        );
        assert!(textual.fault().limit_evidence().is_none());
        assert!(textual.to_string().contains("id \"seven\""));
    }

    #[test]
    fn classify_separates_responses_requests_and_notifications() {
        let response = classify(br#"{"jsonrpc":"2.0","id":3,"result":null}"#).expect("response");
        assert!(response.method.is_none());
        assert_eq!(response.id, Some(serde_json::json!(3)));
        let request = classify(br#"{"jsonrpc":"2.0","id":1,"method":"workspace/configuration"}"#)
            .expect("request");
        assert_eq!(request.method.as_deref(), Some("workspace/configuration"));
        let notification =
            classify(br#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics"}"#)
                .expect("notification");
        assert!(notification.id.is_none());
        assert!(classify(b"{}").is_none());
        assert!(classify(b"not json").is_none());
    }

    fn parsed(payload: &[u8]) -> Value {
        serde_json::from_slice(payload).expect("envelopes are valid JSON")
    }

    #[test]
    fn envelopes_serialize_the_documented_shapes() {
        let mut correlation = Correlation::new();
        let id = correlation.begin("shutdown").expect("id");
        assert_eq!(
            parsed(&request(id, "shutdown", &Value::Null)),
            serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "shutdown", "params": null})
        );
        assert_eq!(
            parsed(&notification("exit", &Value::Null)),
            serde_json::json!({"jsonrpc": "2.0", "method": "exit", "params": null})
        );
        assert_eq!(
            parsed(&response(&serde_json::json!(4), &Value::Null)),
            serde_json::json!({"jsonrpc": "2.0", "id": 4, "result": null})
        );
        assert_eq!(
            parsed(&error_response(
                &serde_json::json!(5),
                METHOD_NOT_FOUND_CODE,
                "not served"
            )),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "error": {"code": METHOD_NOT_FOUND_CODE, "message": "not served"},
            })
        );
    }
}
