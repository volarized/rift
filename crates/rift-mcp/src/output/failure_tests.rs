use rift_error::{ErrorSlug, RiftError, errors};
use rift_protocol::error::{
    ErrorCause, ErrorCode, ErrorData as WireErrorData, ErrorPhase, LimitEvidence, RetryDirective,
};
use rmcp::ErrorData;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, ErrorCode as RpcCode};
use serde_json::json;

use super::ToolFailure;
use crate::failure::{McpErrorExt as _, McpFailure, RIFT_ERROR_CODE, WireFailure as _};
use crate::output::text::OUTPUT_TEXT_BYTES_MAX;

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

/// The only text block of the completed error result `failure` ends in.
fn completed_text(failure: ToolFailure) -> String {
    let Ok(CallToolResponse::Complete(result)) = failure.into_call_tool_result() else {
        panic!("an operating failure completes the call");
    };
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.structured_content, None);
    assert_eq!(result.content.len(), 1, "one text block");
    result.content[0]
        .as_text()
        .expect("a text block")
        .text
        .clone()
}

#[test]
fn a_registered_failure_writes_its_message_and_action_then_its_identity() {
    let failure = errors::ranking::query_empty()
        .mcp()
        .tool_failure(ErrorPhase::Read);
    assert_eq!(
        completed_text(failure),
        [
            "1 error",
            "\tinvalid_request · retry never",
            "\t\tquery is empty; provide query text and resend the request",
            "\t\trift.ranking.query_empty",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn a_registered_failure_without_an_action_writes_its_message_alone() {
    let error = RiftError::new(
        ErrorSlug::new("rift.ranking.query_empty"),
        "query is empty",
        "",
        Vec::new(),
    );
    assert_eq!(error.to_string(), "query is empty; ");
    let failure = McpFailure::new(error).tool_failure(ErrorPhase::Read);
    assert_eq!(
        completed_text(failure),
        [
            "1 error",
            "\tinvalid_request · retry never",
            "\t\tquery is empty",
            "\t\trift.ranking.query_empty",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn a_registered_failure_writes_its_causes_after_its_identity() {
    let error = errors::index::workspace_syntax()
        .cause(
            errors::syntax::source_too_large()
                .source_bytes(2_u64)
                .source_bytes_max(1_u64)
                .error(),
        )
        .error();
    let text = completed_text(McpFailure::new(error).tool_failure(ErrorPhase::Read));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "2 errors", "{text}");
    assert_eq!(lines[1], "\tlimit_exceeded · retry never", "{text}");
    assert_eq!(lines[3], "\t\trift.index.workspace_syntax", "{text}");
    assert_eq!(lines[4], "\tlimit_exceeded · retry never", "{text}");
    assert_eq!(lines.len(), 6, "{text}");
}

#[test]
fn a_registered_failure_whose_text_overflows_stays_its_json_rpc_error() {
    let message = "x".repeat(OUTPUT_TEXT_BYTES_MAX);
    let failure = || {
        RiftError::new(
            ErrorSlug::new("rift.ranking.query_empty"),
            &message,
            "",
            Vec::new(),
        )
    };
    let expected = McpFailure::new(failure()).tool_error(ErrorPhase::Read);
    let Err(passed) = McpFailure::new(failure())
        .tool_failure(ErrorPhase::Read)
        .into_call_tool_result()
    else {
        panic!("a text past the limit stays an error");
    };
    assert_eq!(passed, expected);
}

#[test]
fn a_tool_failure_converts_into_its_json_rpc_error() {
    let registered = || errors::ranking::query_empty().mcp();
    assert_eq!(
        ErrorData::from(registered().tool_failure(ErrorPhase::Read)),
        registered().tool_error(ErrorPhase::Read)
    );
    let routed = ErrorData::new(RpcCode::INTERNAL_ERROR, "protocol failure", None);
    assert_eq!(ErrorData::from(ToolFailure::from(routed.clone())), routed);
}

#[test]
fn a_tool_failure_displays_and_sources_as_the_error_it_carries() {
    use std::error::Error as _;

    let registered = errors::core::configuration_unreadable()
        .file("rift.toml")
        .path("rift.toml")
        .io("permission denied")
        .source(std::io::Error::other("permission denied"))
        .error();
    let shown = registered.to_string();
    let failure = McpFailure::new(registered).tool_failure(ErrorPhase::Read);
    assert_eq!(failure.to_string(), shown);
    assert_eq!(
        failure.source().map(ToString::to_string).as_deref(),
        Some("permission denied")
    );
    let routed = ErrorData::new(RpcCode::INTERNAL_ERROR, "protocol failure", None);
    let failure = ToolFailure::from(routed.clone());
    assert_eq!(failure.to_string(), routed.to_string());
    assert!(failure.source().is_none());
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

#[test]
fn every_registered_error_served_by_a_tool_path_completes_as_an_execution_failure() {
    assert!(
        !errors::REGISTERED_SLUGS.is_empty(),
        "the registry lists errors"
    );
    for slug in errors::REGISTERED_SLUGS {
        let error = RiftError::new(ErrorSlug::new(slug), "message", "action", Vec::new());
        let tool_error = McpFailure::new(error).tool_error(ErrorPhase::Read);
        assert_eq!(tool_error.code, RIFT_ERROR_CODE, "{slug}");
        let Ok(CallToolResponse::Complete(result)) =
            ToolFailure::from(tool_error).into_call_tool_result()
        else {
            panic!("{slug} stays a protocol error");
        };
        assert_eq!(result.is_error, Some(true), "{slug}");
        assert_eq!(result.structured_content, None, "{slug}");
        assert_eq!(result.content.len(), 1, "{slug}");
        let error = RiftError::new(ErrorSlug::new(slug), "message", "action", Vec::new());
        let text = completed_text(McpFailure::new(error).tool_failure(ErrorPhase::Read));
        assert!(
            text.lines().any(|line| line == format!("\t\t{slug}")),
            "{slug} writes its identity: {text}"
        );
    }
}
