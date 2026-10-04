use rift_protocol::error::{
    ErrorCause, ErrorCode, ErrorData as WireErrorData, ErrorPhase, LimitEvidence, RetryDirective,
};
use rmcp::ErrorData;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, ErrorCode as RpcCode};
use serde_json::json;

use super::ToolFailure;
use crate::failure::RIFT_ERROR_CODE;

fn limit_failure() -> WireErrorData {
    WireErrorData {
        code: ErrorCode::LimitExceeded,
        message: "the request crosses a limit".to_owned(),
        retry: RetryDirective::Never,
        phase: ErrorPhase::Read,
        diagnostics: Vec::new(),
        limit: Some(LimitEvidence {
            field: "source.files".to_owned(),
            limit: 10,
            required: 11,
        }),
        causes: vec![ErrorCause {
            code: ErrorCode::StorageFailure,
            message: "the store refused".to_owned(),
            retry: RetryDirective::SameRequest,
        }],
    }
}

/// The error a tool path returns for a typed wire error.
fn tool_path_error(error: &WireErrorData) -> ErrorData {
    ErrorData::new(
        RIFT_ERROR_CODE,
        error.message.clone(),
        serde_json::to_value(error).ok(),
    )
}

#[test]
fn an_operating_failure_completes_as_an_error_result_with_its_text() {
    let failure = ToolFailure::from(tool_path_error(&limit_failure()));
    let Ok(CallToolResponse::Complete(result)) = failure.into_call_tool_result() else {
        panic!("an operating failure completes the call");
    };
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.structured_content, None);
    assert_eq!(result.content.len(), 1, "one text block");
    let block = result.content[0].as_text().expect("a text block");
    assert_eq!(
        block.text,
        [
            "2 errors",
            "\tlimit_exceeded · retry never",
            "\t\tthe request crosses a limit",
            "\t\tlimit source.files: 11 over 10",
            "\tstorage_failure · retry same_request",
            "\t\tthe store refused",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn a_failure_with_another_code_stays_the_json_rpc_error() {
    let data = serde_json::to_value(limit_failure()).expect("wire error serializes");
    let error = ErrorData::new(RpcCode::INTERNAL_ERROR, "protocol failure", Some(data));
    let Err(passed) = ToolFailure::from(error.clone()).into_call_tool_result() else {
        panic!("a non-Rift code must stay an error");
    };
    assert_eq!(passed, error);
}

#[test]
fn a_rift_code_with_data_that_does_not_decode_stays_the_json_rpc_error() {
    for data in [
        None,
        Some(json!({"code": "not_a_code"})),
        Some(json!("text")),
    ] {
        let error = ErrorData::new(RIFT_ERROR_CODE, "undecodable", data);
        let Err(passed) = ToolFailure::from(error.clone()).into_call_tool_result() else {
            panic!("undecodable data must stay an error");
        };
        assert_eq!(passed, error);
    }
}
