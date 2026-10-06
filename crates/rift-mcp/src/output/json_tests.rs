use rift_protocol::error::{ErrorCode, ErrorData as WireErrorData, ErrorPhase, RetryDirective};
use rift_protocol::read::{Pagination, ReadWarning};
use rift_protocol::search::SearchResult;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, CallToolResult};
use serde::ser::Error as _;
use serde::{Serialize, Serializer};

use super::render::{RegisteredFailure, Render, text_of};
use super::text::{OutputOverflow, TextError, TextWriter};
use super::{AnswerFault, Json, answer_error, resource_text};
use crate::failure::{McpErrorExt as _, WireFailure as _};

fn search_answer() -> SearchResult {
    SearchResult {
        results: Vec::new(),
        pagination: Pagination {
            page_index: 0,
            total_pages: 0,
        },
        warnings: vec![ReadWarning::GlobalAccessDisabled],
    }
}

/// An answer whose text writer fails with the given error.
struct RefusedText(TextError);

impl Render for RefusedText {
    fn render(&self, _out: &mut TextWriter) -> Result<(), TextError> {
        Err(self.0.clone())
    }
}

impl Serialize for RefusedText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_unit()
    }
}

/// An answer whose text renders and whose structured form fails.
struct RefusedStructure;

impl Render for RefusedStructure {
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        out.raw_line(0, "rows")
    }
}

impl Serialize for RefusedStructure {
    fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(S::Error::custom("no structured form"))
    }
}

/// The completed error result an answer that cannot be served ends in.
fn failure_of<T: Render + Serialize>(answer: T) -> CallToolResult {
    let Ok(CallToolResponse::Complete(result)) = Json(answer).into_call_tool_result() else {
        panic!("the answer must end in a completed error result");
    };
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.structured_content, None);
    result
}

/// The only text block of a completed error result.
fn text_of_failure(result: &CallToolResult) -> &str {
    assert_eq!(result.content.len(), 1, "one text block");
    &result.content[0].as_text().expect("a text block").text
}

#[test]
fn an_answer_serves_compact_text_and_the_same_value_as_structured_content() {
    let answer = search_answer();
    let expected_text = text_of(&answer).expect("answer renders");
    let expected_value = serde_json::to_value(&answer).expect("answer serializes");
    let Ok(CallToolResponse::Complete(result)) = Json(answer).into_call_tool_result() else {
        panic!("a rendered answer completes the call");
    };
    assert_eq!(result.content.len(), 1, "one text block");
    let block = result.content[0].as_text().expect("a text block");
    assert_eq!(
        block.text,
        "0 results\n1 warning\n\tglobal_access_disabled\n"
    );
    assert_eq!(block.text, expected_text);
    assert_eq!(result.structured_content, Some(expected_value));
    assert_eq!(result.is_error, Some(false));
}

#[test]
fn overflow_is_limit_exceeded_with_retry_never_and_names_the_limit() {
    let overflow = TextError::Overflow(OutputOverflow { limit: 64 });
    let result = failure_of(RefusedText(overflow));
    assert_eq!(
        text_of_failure(&result),
        "1 error\n\tlimit_exceeded · retry never\n\t\tanswer text exceeds its accepted limit of 64 \
         bytes; narrow the request or lower `limit`, then resend the request\n\t\t\
         rift.mcp.answer_text_limit\n",
        "the writer does not know the required size, so no limit line or causes"
    );
}

#[test]
fn another_render_failure_is_internal_error_with_the_writer_error_as_cause() {
    let result = failure_of(RefusedText(TextError::Unsupported("bytes")));
    assert_eq!(
        text_of_failure(&result),
        "2 errors\n\tinternal_error · retry same_request\n\t\tanswer text could not be written: \
         unsupported shape in text: bytes; report this internal failure with its full context\n\t\t\
         rift.mcp.answer_text_failed\n\tinternal_error · retry same_request\n\t\tunsupported shape in text: bytes\n"
    );
}

#[test]
fn a_serialization_failure_is_internal_error_with_the_serde_error_as_cause() {
    let result = failure_of(RefusedStructure);
    assert_eq!(
        text_of_failure(&result),
        "2 errors\n\tinternal_error · retry same_request\n\t\tanswer could not be serialized into \
         structured content: no structured form; report this internal failure with its full \
         context\n\t\trift.mcp.answer_structure_failed\n\tinternal_error · retry same_request\n\t\t\
         no structured form\n"
    );
}

#[test]
fn the_error_text_is_the_rendering_of_the_registered_error_as_wire_data_with_its_identity() {
    let fault = AnswerFault::Text(TextError::Custom("boom".to_owned()));
    let error = answer_error(fault);
    let identity = error.slug().as_str();
    let wire: WireErrorData = error.mcp().wire_error(ErrorPhase::Read);
    let expected = text_of(&RegisteredFailure {
        identity,
        error: &wire,
    })
    .expect("the registered failure renders");
    let result = failure_of(RefusedText(TextError::Custom("boom".to_owned())));
    assert_eq!(text_of_failure(&result), expected);
    assert_eq!(wire.retry, RetryDirective::SameRequest);
    assert_eq!(wire.phase, ErrorPhase::Read);
    assert_eq!(wire.code, ErrorCode::InternalError);
}

#[test]
fn overflow_wire_data_carries_no_limit_record() {
    let fault = AnswerFault::Text(TextError::Overflow(OutputOverflow { limit: 64 }));
    let wire = answer_error(fault).mcp().wire_error(ErrorPhase::Read);
    assert_eq!(wire.code, ErrorCode::LimitExceeded);
    assert_eq!(wire.retry, RetryDirective::Never);
    assert_eq!(wire.limit, None, "the required size is unknown");
    assert!(wire.causes.is_empty());
}

#[test]
fn a_resource_text_is_the_rendering_of_the_answer() {
    let answer = search_answer();
    let expected = text_of(&answer).expect("answer renders");
    assert_eq!(resource_text(&answer).expect("text renders"), expected);
}

#[test]
fn a_refused_resource_text_is_the_registered_failure_as_a_json_rpc_error() {
    let overflow = TextError::Overflow(OutputOverflow { limit: 64 });
    let error = resource_text(&RefusedText(overflow.clone())).expect_err("the text is refused");
    let expected = answer_error(AnswerFault::Text(overflow))
        .mcp()
        .tool_error(ErrorPhase::Read);
    assert_eq!(error, expected);
    assert_eq!(error.code, crate::failure::RIFT_ERROR_CODE);
    let data: WireErrorData =
        serde_json::from_value(error.data.expect("the error carries wire data"))
            .expect("the data is the typed wire error");
    assert_eq!(data.code, ErrorCode::LimitExceeded);
    assert_eq!(data.phase, ErrorPhase::Read);
}
