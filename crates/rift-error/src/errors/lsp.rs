error_namespace! {
    lsp;

    capabilities_position_encoding_unsupported {
        message = "language engine selected unsupported position encoding {encoding}",
        action = "configure the engine to use UTF-8 or UTF-16 positions",
        fields = {
            encoding: required(string),
        },
    }

    correlation_pending_requests_exceeded {
        message = "language engine has more than {limit} pending requests",
        action = "wait for a pending request to finish before sending another",
        fields = {
            field: required(string),
            limit: required(unsigned),
            required: required(unsigned),
        },
    }

    correlation_response_unknown {
        message = "language engine answered unknown request id {id}",
        action = "check the language engine's request and response correlation",
        fields = {
            id: required(string),
        },
    }

    engine_analyzing {
        message = "language engine remained analyzing after {attempts} attempts",
        action = "wait for language engine analysis to finish and retry",
        fields = {
            attempts: required(unsigned),
        },
    }

    engine_capability_absent {
        message = "language engine does not advertise {capability}",
        action = "configure an engine that advertises {capability}",
        fields = {
            capability: required(string),
        },
    }

    engine_connection_closed {
        message = "language engine closed connection during {method}",
        action = "restart the language engine and retry",
        fields = {
            method: required(string),
        },
    }

    engine_ended {
        message = "language engine session has ended",
        action = "start a new language engine session",
        fields = {},
    }

    engine_launch_failed {
        message = "language engine could not start",
        action = "check the configured program and process limits",
        fields = {
            source: required(source),
        },
    }

    engine_message_unreadable {
        message = "language engine response is not a JSON-RPC envelope",
        action = "check the language engine's LSP response",
        fields = {},
    }

    engine_program_absolute {
        message = "language engine program {program} is an absolute path",
        action = "set a program name resolved from the inherited PATH",
        fields = {
            program: required(string),
        },
    }

    engine_program_empty {
        message = "language engine program is empty",
        action = "set a program for the language engine",
        fields = {},
    }

    engine_refused_retryable {
        message = "language engine refused {method} with code {code}: {message}",
        action = "resend the same request after the engine finishes its current work",
        fields = {
            code: required(integer),
            message: required(string),
            method: required(string),
        },
    }

    engine_refused_terminal {
        message = "language engine refused {method} with code {code}: {message}",
        action = "correct the request and retry",
        fields = {
            code: required(integer),
            message: required(string),
            method: required(string),
        },
    }

    engine_result_invalid {
        message = "language engine response for {method} has an invalid result",
        action = "check the language engine's response for {method}",
        fields = {
            method: required(string),
            source: required(source),
        },
    }

    engine_timed_out {
        message = "language engine timed out during {method} after {timeout_ms} ms",
        action = "retry the request after checking language engine load",
        fields = {
            method: required(string),
            timeout_ms: required(unsigned),
        },
    }

    framing_content_length_invalid {
        message = "language engine message has invalid Content-Length {value}",
        action = "send Content-Length as a decimal byte count",
        fields = {
            value: required(string),
        },
    }

    framing_content_length_missing {
        message = "language engine message header has no Content-Length",
        action = "check the language engine's LSP framing and retry",
        fields = {},
    }

    framing_header_malformed {
        message = "language engine message header is malformed",
        action = "check the language engine's LSP framing and retry",
        fields = {},
    }

    framing_header_too_long {
        message = "language engine message header exceeds its byte limit",
        action = "reduce the message header and retry",
        fields = {},
    }

    framing_message_too_long {
        message = "language engine message body of {announced_bytes} bytes exceeds limit {limit}",
        action = "reduce the message body below {limit} bytes and retry",
        fields = {
            announced_bytes: required(unsigned),
            field: required(string),
            limit: required(unsigned),
            required: required(unsigned),
        },
    }

    position_character_misaligned {
        message = "character {character} splits an encoded character on line {line}",
        action = "use a character offset on an encoding boundary",
        fields = {
            character: required(unsigned),
            line: required(unsigned),
        },
    }

    position_character_out_of_range {
        message = "character {character} is outside line {line} length {line_units}",
        action = "use a character offset within the line",
        fields = {
            character: required(unsigned),
            line: required(unsigned),
            line_units: required(unsigned),
        },
    }

    position_line_out_of_range {
        message = "position line {line} is outside document line count {line_count}",
        action = "use a line within the document",
        fields = {
            line: required(unsigned),
            line_count: required(unsigned),
        },
    }

    position_offset_inside_line_ending {
        message = "byte offset {byte_offset} falls inside a line ending",
        action = "use a byte offset before or after the line ending",
        fields = {
            byte_offset: required(unsigned),
        },
    }

    position_offset_misaligned {
        message = "byte offset {byte_offset} splits a UTF-8 character",
        action = "use a byte offset on a UTF-8 boundary",
        fields = {
            byte_offset: required(unsigned),
        },
    }

    position_offset_out_of_range {
        message = "byte offset {byte_offset} exceeds document size {document_bytes}",
        action = "use a byte offset within the document",
        fields = {
            byte_offset: required(unsigned),
            document_bytes: required(unsigned),
        },
    }

    uri_host_refused {
        message = "document URI host {host} is not supported",
        action = "use a hostless file URI for the document",
        fields = {
            host: required(string),
        },
    }

    uri_outside_root {
        message = "document URI is outside the workspace root",
        action = "use a document URI under the workspace root",
        fields = {},
    }

    uri_path_not_decodable {
        message = "document URI path does not decode to Unicode",
        action = "use a file URI with a UTF-8 path",
        fields = {},
    }

    uri_root_not_absolute {
        message = "language engine workspace root {root} is not absolute",
        action = "configure an absolute workspace root",
        fields = {
            root: required(string),
        },
    }

    uri_root_not_unicode {
        message = "language engine workspace root is not valid Unicode",
        action = "use a workspace root with a Unicode path",
        fields = {},
    }

    uri_scheme_refused {
        message = "document URI scheme {scheme} is not supported",
        action = "use a file URI for the document",
        fields = {
            scheme: required(string),
        },
    }

    uri_uri_malformed {
        message = "language engine returned malformed document URI {uri}",
        action = "use a valid file URI for the document",
        fields = {
            uri: required(string),
        },
    }
}
