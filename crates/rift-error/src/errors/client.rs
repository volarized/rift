use rift_error::__rift_error_definition;

__rift_error_definition!(
    body_limit,
    slug = "rift.client.body_limit",
    message = "global client {direction} body exceeds its accepted limit of {maximum} bytes; observed {bytes}",
    action = "narrow the request or adjust the accepted byte limit, then resend the request",
    fields = {
        bytes: required(unsigned),
        direction: required(string),
        maximum: required(unsigned),
        operation: required(string),
    },
);

__rift_error_definition!(
    configuration_invalid,
    slug = "rift.client.configuration_invalid",
    message = "global client configuration field {field} failed validation: {violation}",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: required(string),
        source: optional(source),
        violation: required(string),
    },
);

__rift_error_definition!(
    remote_failed,
    slug = "rift.client.remote_failed",
    message = "global client endpoint returned a non-success response during {operation}: HTTP status {status}",
    action = "report this internal failure with its full context",
    fields = {
        metadata: required(string),
        operation: required(string),
        problem: optional(string),
        source: optional(source),
        status: required(unsigned),
    },
);

__rift_error_definition!(
    remote_permission_denied,
    slug = "rift.client.remote_permission_denied",
    message = "global client endpoint refused permission during {operation}: HTTP status {status}",
    action = "correct the credential configuration, then retry",
    fields = {
        metadata: required(string),
        operation: required(string),
        problem: optional(string),
        source: optional(source),
        status: required(unsigned),
    },
);

__rift_error_definition!(
    remote_unavailable,
    slug = "rift.client.remote_unavailable",
    message = "global client endpoint cannot serve {operation} now: HTTP status {status}",
    action = "resend the same request after the reported retry delay",
    fields = {
        metadata: required(string),
        operation: required(string),
        problem: optional(string),
        source: optional(source),
        status: required(unsigned),
    },
);

__rift_error_definition!(
    request_cancelled,
    slug = "rift.client.request_cancelled",
    message = "global client request was cancelled during {operation}",
    action = "resend the request if the result is still needed",
    fields = {
        operation: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    request_failed,
    slug = "rift.client.request_failed",
    message = "global client request failed during {operation}: {reason}",
    action = "resend the same request after a short delay",
    fields = {
        operation: required(string),
        reason: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    request_invalid,
    slug = "rift.client.request_invalid",
    message = "global client request field {field} failed validation: {violation}",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
        operation: required(string),
        source: optional(source),
        violation: required(string),
    },
);

__rift_error_definition!(
    response_invalid,
    slug = "rift.client.response_invalid",
    message = "global client response field {field} failed validation during {operation}: {violation}",
    action = "report this internal failure with its full context",
    fields = {
        field: required(string),
        operation: required(string),
        source: optional(source),
        status: optional(unsigned),
        violation: required(string),
    },
);
