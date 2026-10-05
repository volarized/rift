use rift_error::__rift_error_definition;

__rift_error_definition!(
    answer_structure_failed,
    slug = "rift.mcp.answer_structure_failed",
    message = "answer could not be serialized into structured content: {source}",
    action = "report this internal failure with its full context",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    answer_text_failed,
    slug = "rift.mcp.answer_text_failed",
    message = "answer text could not be written: {source}",
    action = "report this internal failure with its full context",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    answer_text_limit,
    slug = "rift.mcp.answer_text_limit",
    message = "answer text exceeds its accepted limit of {limit} bytes",
    action = "narrow the request or lower `limit`, then resend the request",
    fields = {
        limit: required(unsigned),
    },
);

__rift_error_definition!(
    arguments_not_object,
    slug = "rift.mcp.arguments_not_object",
    message = "tool call arguments are not a JSON object",
    action = "send tool call arguments as a JSON object and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    election_already_serving,
    slug = "rift.mcp.election_already_serving",
    message = "another process already serves this workspace",
    action = "connect to the serving process or stop it before starting another",
    fields = {},
);

__rift_error_definition!(
    election_document_invalid,
    slug = "rift.mcp.election_document_invalid",
    message = "server lock document failed validation",
    action = "report this internal failure with its full context",
    fields = {},
);

__rift_error_definition!(
    election_storage_failed,
    slug = "rift.mcp.election_storage_failed",
    message = "workspace server election state could not be read or written",
    action = "check filesystem permissions and free space, then retry",
    fields = {
        operation: required(string),
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    forward_unanswered,
    slug = "rift.mcp.forward_unanswered",
    message = "workspace server did not answer forwarded request within {waited}",
    action = "resend the same request after a short delay",
    fields = {
        waited: required(duration),
    },
);

__rift_error_definition!(
    http_ports_exhausted,
    slug = "rift.mcp.http_ports_exhausted",
    message = "every loopback port in the serving range is bound",
    action = "stop a process using a serving port, then retry",
    fields = {
        port_max: required(port),
        port_min: required(port),
    },
);

__rift_error_definition!(
    http_serve_failed,
    slug = "rift.mcp.http_serve_failed",
    message = "HTTP MCP server failed while serving",
    action = "report this internal failure with its full context",
    fields = {
        cause: optional(cause),
        operation: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    parameter_invalid,
    slug = "rift.mcp.parameter_invalid",
    message = "tool arguments do not match the served schema",
    action = "correct the reported field and resend the request",
    fields = {
        accepted: optional(string),
        example: optional(string),
        field: optional(string),
        tool: required(string),
    },
);

__rift_error_definition!(
    project_hit_identity_missing,
    slug = "rift.mcp.project_hit_identity_missing",
    message = "project hit has no identity",
    action = "report this internal failure with its full context",
    fields = {
        hit: required(string),
    },
);

__rift_error_definition!(
    project_hit_identity_refused,
    slug = "rift.mcp.project_hit_identity_refused",
    message = "project hit identity was refused by ranking",
    action = "report this internal failure with its full context",
    fields = {
        cause: optional(cause),
        hit: required(string),
    },
);

__rift_error_definition!(
    project_hit_identity_undecodable,
    slug = "rift.mcp.project_hit_identity_undecodable",
    message = "project hit identity is not a qualified name",
    action = "report this internal failure with its full context",
    fields = {
        hit: required(string),
    },
);

__rift_error_definition!(
    project_hit_name_unmatched,
    slug = "rift.mcp.project_hit_name_unmatched",
    message = "project hit name does not match requested name",
    action = "report this internal failure with its full context",
    fields = {
        hit: required(string),
    },
);

__rift_error_definition!(
    project_hit_unit_invalid,
    slug = "rift.mcp.project_hit_unit_invalid",
    message = "project hit unit is not a source unit address",
    action = "report this internal failure with its full context",
    fields = {
        hit: required(string),
    },
);

__rift_error_definition!(
    proxy_identity_failed,
    slug = "rift.mcp.proxy_identity_failed",
    message = "MCP proxy could not determine product identity",
    action = "report this internal failure with its full context",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    proxy_initialization_failed,
    slug = "rift.mcp.proxy_initialization_failed",
    message = "MCP proxy initialization failed",
    action = "resend the same request after a short delay",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    proxy_task_failed,
    slug = "rift.mcp.proxy_task_failed",
    message = "MCP proxy service task failed",
    action = "report this internal failure with its full context",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    proxy_unexpected_quit,
    slug = "rift.mcp.proxy_unexpected_quit",
    message = "MCP service ended unexpectedly",
    action = "report this internal failure with its full context",
    fields = {},
);

__rift_error_definition!(
    spawn_failed,
    slug = "rift.mcp.spawn_failed",
    message = "spawned server exited before serving",
    action = "report this internal failure with its full context",
    fields = {
        stderr: required(string, sensitive),
        stderr_truncated: optional(bool),
    },
);

__rift_error_definition!(
    spawn_no_output,
    slug = "rift.mcp.spawn_no_output",
    message = "spawned server exited before serving and wrote no standard error",
    action = "report this internal failure with its full context",
    fields = {},
);

__rift_error_definition!(
    start_building,
    slug = "rift.mcp.start_building",
    message = "workspace server did not finish building its first index within {waited}",
    action = "resend the same request after a short delay",
    fields = {
        waited: required(duration),
    },
);
