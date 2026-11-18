use rift_error::RiftError;

use crate::{ProblemDetails, ResponseMeta};

pub(crate) fn remote(
    operation: &str,
    meta: &ResponseMeta,
    problem: Option<&ProblemDetails>,
) -> RiftError {
    // The contract documents 400 for a request the global API refuses as invalid, so the
    // caller reads it as its own invalid request.
    if meta.status == 400 {
        let (field, violation) = refused_field(problem);
        return request_invalid(operation, &field, &violation);
    }
    if unsupported_operation(operation, meta, problem) {
        return rift_error::errors::server::read_unsupported()
            .capability("global edge operation")
            .error();
    }
    let metadata = serde_json::json!({
        "status": meta.status,
        "content_type": meta.content_type,
        "etag": meta.etag,
        "cache_control": meta.cache_control,
        "www_authenticate": meta.www_authenticate,
        "retry_after": meta.retry_after,
        "rate_limit_limit": meta.rate_limit_limit,
        "rate_limit_remaining": meta.rate_limit_remaining,
        "rate_limit_reset": meta.rate_limit_reset,
    })
    .to_string();
    let problem = problem.map(|value| {
        let mut fields: serde_json::Map<String, serde_json::Value> =
            value.additional_properties.clone().into_iter().collect();
        fields.insert("type".to_owned(), serde_json::json!(value.r#type));
        fields.insert("title".to_owned(), serde_json::json!(value.title));
        fields.insert("status".to_owned(), serde_json::json!(value.status));
        fields.insert("detail".to_owned(), serde_json::json!(value.detail));
        if let Some(instance) = &value.instance {
            fields.insert("instance".to_owned(), serde_json::json!(instance));
        }
        serde_json::Value::Object(fields).to_string()
    });
    match meta.status {
        401 | 403 => rift_error::errors::client::remote_permission_denied()
            .operation(operation)
            .status(u64::from(meta.status))
            .metadata(metadata)
            .maybe_problem(problem)
            .error(),
        429 | 502 | 503 | 504 => rift_error::errors::client::remote_unavailable()
            .operation(operation)
            .status(u64::from(meta.status))
            .metadata(metadata)
            .maybe_problem(problem)
            .error(),
        _ => rift_error::errors::client::remote_failed()
            .operation(operation)
            .status(u64::from(meta.status))
            .metadata(metadata)
            .maybe_problem(problem)
            .error(),
    }
}

/// The form the global API's read refusal message takes; the field and violation follow.
const REFUSAL_PREFIX: &str = "the request does not match the documented form: field ";
/// What separates the field from the violation in that message.
const REFUSAL_VIOLATION: &str = ", violation ";

/// The request field and violation a 400 refusal names: its `field` and `violation`
/// members, else the ones its `detail` states in the registered refusal message, else the
/// whole `detail` against the request.
fn refused_field(problem: Option<&ProblemDetails>) -> (String, String) {
    let Some(problem) = problem else {
        return (
            "request".to_owned(),
            "the global API refused the request".to_owned(),
        );
    };
    let member = |name: &str| {
        problem
            .additional_properties
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
    };
    if let (Some(field), Some(violation)) = (member("field"), member("violation")) {
        return (field.to_owned(), violation.to_owned());
    }
    problem
        .detail
        .strip_prefix(REFUSAL_PREFIX)
        .and_then(|rest| rest.split_once(REFUSAL_VIOLATION))
        .filter(|(field, violation)| !field.is_empty() && !violation.is_empty())
        .map_or_else(
            || ("request".to_owned(), problem.detail.clone()),
            |(field, violation)| (field.to_owned(), violation.to_owned()),
        )
}

/// Whether `error` refuses one request rather than stating the endpoint's state: the global
/// API answered 400 for it, or the request outgrew a body bound.
pub(crate) fn refuses_request_alone(error: &RiftError) -> bool {
    let slug = error.slug();
    slug == rift_error::errors::client::request_invalid::SLUG
        || (slug == rift_error::errors::client::body_limit::SLUG
            && error.fields().iter().any(|field| {
                field.key() == "direction"
                    && matches!(field.value(), rift_error::ErrorValue::Display(value) if value == "request")
            }))
}

pub(crate) fn unsupported_operation(
    operation: &str,
    meta: &ResponseMeta,
    problem: Option<&ProblemDetails>,
) -> bool {
    operation == crate::contract::Endpoint::Search.operation_id()
        && meta.status == 503
        && problem.is_some_and(|problem| {
            problem
                .additional_properties
                .get("code")
                .and_then(serde_json::Value::as_str)
                == Some("rift.server.read_unsupported")
        })
}

pub(crate) fn request_invalid(operation: &str, field: &str, violation: &str) -> RiftError {
    rift_error::errors::client::request_invalid()
        .operation(operation)
        .field(field)
        .violation(violation)
        .error()
}

pub(crate) fn request_invalid_source(
    operation: &str,
    field: &str,
    violation: &str,
    source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> RiftError {
    rift_error::errors::client::request_invalid()
        .operation(operation)
        .field(field)
        .violation(violation)
        .source(source)
        .error()
}

pub(crate) fn response_invalid(
    operation: &str,
    field: &str,
    violation: &str,
    status: Option<u16>,
) -> RiftError {
    rift_error::errors::client::response_invalid()
        .operation(operation)
        .field(field)
        .violation(violation)
        .maybe_status(status.map(u64::from))
        .error()
}

pub(crate) fn response_source(
    operation: &str,
    field: &str,
    violation: &str,
    status: Option<u16>,
    source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> RiftError {
    rift_error::errors::client::response_invalid()
        .operation(operation)
        .field(field)
        .violation(violation)
        .maybe_status(status.map(u64::from))
        .source(source)
        .error()
}

pub(crate) fn body_limit(
    operation: &str,
    direction: &str,
    bytes: usize,
    maximum: usize,
) -> RiftError {
    rift_error::errors::client::body_limit()
        .operation(operation)
        .direction(direction)
        .bytes(u64::try_from(bytes).unwrap_or(u64::MAX))
        .maximum(u64::try_from(maximum).unwrap_or(u64::MAX))
        .error()
}

#[cfg(test)]
pub(crate) fn request_failed(operation: &str, reason: &str) -> RiftError {
    rift_error::errors::client::request_failed()
        .operation(operation)
        .reason(reason)
        .error()
}

pub(crate) fn request_source(
    operation: &str,
    reason: &str,
    source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> RiftError {
    rift_error::errors::client::request_failed()
        .operation(operation)
        .reason(reason)
        .source(source)
        .error()
}

#[cfg(test)]
pub(crate) fn request_cancelled(operation: &str) -> RiftError {
    rift_error::errors::client::request_cancelled()
        .operation(operation)
        .error()
}

pub(crate) fn request_cancelled_source(
    operation: &str,
    source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> RiftError {
    rift_error::errors::client::request_cancelled()
        .operation(operation)
        .source(source)
        .error()
}

pub(crate) fn retryable_connection(error: &RiftError) -> bool {
    error.slug().as_str() == "rift.client.request_failed"
        && error.fields().iter().any(|field| {
            field.key() == "reason"
                && matches!(
                    field.value(),
                    rift_error::ErrorValue::Display(value) if value == "global endpoint connection failed"
                )
        })
}

pub(crate) fn configuration_invalid(field: &str, violation: &str) -> RiftError {
    rift_error::errors::client::configuration_invalid()
        .field(field)
        .violation(violation)
        .error()
}

pub(crate) fn configuration_source(
    field: &str,
    violation: &str,
    source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> RiftError {
    rift_error::errors::client::configuration_invalid()
        .field(field)
        .violation(violation)
        .source(source)
        .error()
}
