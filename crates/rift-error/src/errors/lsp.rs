use rift_error::__rift_error_definition;

__rift_error_definition!(
    capabilities_position_encoding_unsupported,
    slug = "rift.lsp.capabilities_position_encoding_unsupported",
    message = "language engine selected unsupported position encoding {encoding}",
    action = "configure the engine to use UTF-8 or UTF-16 positions",
    fields = {
        encoding: required(string),
    },
);

__rift_error_definition!(
    correlation_pending_requests_exceeded,
    slug = "rift.lsp.correlation_pending_requests_exceeded",
    message = "language engine has more than {limit} pending requests",
    action = "wait for a pending request to finish before sending another",
    fields = {
        field: required(string),
        limit: required(unsigned),
        required: required(unsigned),
    },
);

__rift_error_definition!(
    correlation_response_unknown,
    slug = "rift.lsp.correlation_response_unknown",
    message = "language engine answered unknown request id {id}",
    action = "check the language engine's request and response correlation",
    fields = {
        id: required(string),
    },
);

__rift_error_definition!(
    engine_analyzing,
    slug = "rift.lsp.engine_analyzing",
    message = "language engine remained analyzing after {attempts} attempts",
    action = "wait for language engine analysis to finish and retry",
    fields = {
        attempts: required(unsigned),
    },
);

__rift_error_definition!(
    engine_capability_absent,
    slug = "rift.lsp.engine_capability_absent",
    message = "language engine does not advertise {capability}",
    action = "configure an engine that advertises {capability}",
    fields = {
        capability: required(string),
    },
);

__rift_error_definition!(
    engine_connection_closed,
    slug = "rift.lsp.engine_connection_closed",
    message = "language engine closed connection during {method}",
    action = "restart the language engine and retry",
    fields = {
        method: required(string),
    },
);

__rift_error_definition!(
    engine_ended,
    slug = "rift.lsp.engine_ended",
    message = "language engine session has ended",
    action = "start a new language engine session",
    fields = {},
);

__rift_error_definition!(
    engine_launch_failed,
    slug = "rift.lsp.engine_launch_failed",
    message = "language engine could not start",
    action = "check the configured program and process limits",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    engine_message_unreadable,
    slug = "rift.lsp.engine_message_unreadable",
    message = "language engine response is not a JSON-RPC envelope",
    action = "check the language engine's LSP response",
    fields = {},
);

__rift_error_definition!(
    engine_program_absolute,
    slug = "rift.lsp.engine_program_absolute",
    message = "language engine program {program} is an absolute path",
    action = "set a program name resolved from the inherited PATH",
    fields = {
        program: required(string),
    },
);

__rift_error_definition!(
    engine_program_empty,
    slug = "rift.lsp.engine_program_empty",
    message = "language engine program is empty",
    action = "set a program for the language engine",
    fields = {},
);

__rift_error_definition!(
    engine_refused_retryable,
    slug = "rift.lsp.engine_refused_retryable",
    message = "language engine refused {method} with code {code}: {message}",
    action = "resend the same request after the engine finishes its current work",
    fields = {
        code: required(integer),
        message: required(string),
        method: required(string),
    },
);

__rift_error_definition!(
    engine_refused_terminal,
    slug = "rift.lsp.engine_refused_terminal",
    message = "language engine refused {method} with code {code}: {message}",
    action = "correct the request and retry",
    fields = {
        code: required(integer),
        message: required(string),
        method: required(string),
    },
);

__rift_error_definition!(
    engine_result_invalid,
    slug = "rift.lsp.engine_result_invalid",
    message = "language engine response for {method} has an invalid result",
    action = "check the language engine's response for {method}",
    fields = {
        method: required(string),
        source: required(source),
    },
);

__rift_error_definition!(
    engine_timed_out,
    slug = "rift.lsp.engine_timed_out",
    message = "language engine timed out during {method} after {timeout_ms} ms",
    action = "retry the request after checking language engine load",
    fields = {
        method: required(string),
        timeout_ms: required(unsigned),
    },
);

__rift_error_definition!(
    framing_content_length_invalid,
    slug = "rift.lsp.framing_content_length_invalid",
    message = "language engine message has invalid Content-Length {value}",
    action = "send Content-Length as a decimal byte count",
    fields = {
        value: required(string),
    },
);

__rift_error_definition!(
    framing_content_length_missing,
    slug = "rift.lsp.framing_content_length_missing",
    message = "language engine message header has no Content-Length",
    action = "check the language engine's LSP framing and retry",
    fields = {},
);

__rift_error_definition!(
    framing_header_malformed,
    slug = "rift.lsp.framing_header_malformed",
    message = "language engine message header is malformed",
    action = "check the language engine's LSP framing and retry",
    fields = {},
);

__rift_error_definition!(
    framing_header_too_long,
    slug = "rift.lsp.framing_header_too_long",
    message = "language engine message header exceeds its byte limit",
    action = "reduce the message header and retry",
    fields = {},
);

__rift_error_definition!(
    framing_message_too_long,
    slug = "rift.lsp.framing_message_too_long",
    message = "language engine message body of {announced_bytes} bytes exceeds limit {limit}",
    action = "reduce the message body below {limit} bytes and retry",
    fields = {
        announced_bytes: required(unsigned),
        field: required(string),
        limit: required(unsigned),
        required: required(unsigned),
    },
);

__rift_error_definition!(
    position_character_misaligned,
    slug = "rift.lsp.position_character_misaligned",
    message = "character {character} splits an encoded character on line {line}",
    action = "use a character offset on an encoding boundary",
    fields = {
        character: required(unsigned),
        line: required(unsigned),
    },
);

__rift_error_definition!(
    position_character_out_of_range,
    slug = "rift.lsp.position_character_out_of_range",
    message = "character {character} is outside line {line} length {line_units}",
    action = "use a character offset within the line",
    fields = {
        character: required(unsigned),
        line: required(unsigned),
        line_units: required(unsigned),
    },
);

__rift_error_definition!(
    position_line_out_of_range,
    slug = "rift.lsp.position_line_out_of_range",
    message = "position line {line} is outside document line count {line_count}",
    action = "use a line within the document",
    fields = {
        line: required(unsigned),
        line_count: required(unsigned),
    },
);

__rift_error_definition!(
    position_offset_inside_line_ending,
    slug = "rift.lsp.position_offset_inside_line_ending",
    message = "byte offset {byte_offset} falls inside a line ending",
    action = "use a byte offset before or after the line ending",
    fields = {
        byte_offset: required(unsigned),
    },
);

__rift_error_definition!(
    position_offset_misaligned,
    slug = "rift.lsp.position_offset_misaligned",
    message = "byte offset {byte_offset} splits a UTF-8 character",
    action = "use a byte offset on a UTF-8 boundary",
    fields = {
        byte_offset: required(unsigned),
    },
);

__rift_error_definition!(
    position_offset_out_of_range,
    slug = "rift.lsp.position_offset_out_of_range",
    message = "byte offset {byte_offset} exceeds document size {document_bytes}",
    action = "use a byte offset within the document",
    fields = {
        byte_offset: required(unsigned),
        document_bytes: required(unsigned),
    },
);

__rift_error_definition!(
    uri_host_refused,
    slug = "rift.lsp.uri_host_refused",
    message = "document URI host {host} is not supported",
    action = "use a hostless file URI for the document",
    fields = {
        host: required(string),
    },
);

__rift_error_definition!(
    uri_outside_root,
    slug = "rift.lsp.uri_outside_root",
    message = "document URI is outside the workspace root",
    action = "use a document URI under the workspace root",
    fields = {},
);

__rift_error_definition!(
    uri_path_not_decodable,
    slug = "rift.lsp.uri_path_not_decodable",
    message = "document URI path does not decode to Unicode",
    action = "use a file URI with a UTF-8 path",
    fields = {},
);

__rift_error_definition!(
    uri_root_not_absolute,
    slug = "rift.lsp.uri_root_not_absolute",
    message = "language engine workspace root {root} is not absolute",
    action = "configure an absolute workspace root",
    fields = {
        root: required(string),
    },
);

__rift_error_definition!(
    uri_root_not_unicode,
    slug = "rift.lsp.uri_root_not_unicode",
    message = "language engine workspace root is not valid Unicode",
    action = "use a workspace root with a Unicode path",
    fields = {},
);

__rift_error_definition!(
    uri_scheme_refused,
    slug = "rift.lsp.uri_scheme_refused",
    message = "document URI scheme {scheme} is not supported",
    action = "use a file URI for the document",
    fields = {
        scheme: required(string),
    },
);

__rift_error_definition!(
    uri_uri_malformed,
    slug = "rift.lsp.uri_uri_malformed",
    message = "language engine returned malformed document URI {uri}",
    action = "use a valid file URI for the document",
    fields = {
        uri: required(string),
    },
);
