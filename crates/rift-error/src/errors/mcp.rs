error_namespace! {
    mcp;

    answer_structure_failed {
        message = "answer could not be serialized into structured content: {source}",
        action = "report this internal failure with its full context",
        fields = {
            source: required(source),
        },
    }

    answer_text_failed {
        message = "answer text could not be written: {source}",
        action = "report this internal failure with its full context",
        fields = {
            source: required(source),
        },
    }

    answer_text_limit {
        message = "answer text exceeds its accepted limit of {limit} bytes",
        action = "narrow the request or lower `limit`, then resend the request",
        fields = {
            limit: required(unsigned),
        },
    }

    arguments_not_object {
        message = "tool call arguments are not a JSON object",
        action = "send tool call arguments as a JSON object and resend the request",
        fields = {
            field: required(string),
        },
    }

    election_already_serving {
        message = "another process already serves this workspace",
        action = "connect to the serving process or stop it before starting another",
        fields = {},
    }

    election_document_invalid {
        message = "server lock document failed validation",
        action = "report this internal failure with its full context",
        fields = {},
    }

    election_storage_failed {
        message = "workspace server election state could not be read or written",
        action = "check filesystem permissions and free space, then retry",
        fields = {
            operation: required(string),
            path: required(path),
            source: required(source),
        },
    }

    feedback_intake_unavailable {
        message = "feedback intake is unavailable",
        action = "set RIFT_FEEDBACK_URL to an anonymous HTTPS URL, or unset it for the public intake, then restart the workspace server",
        fields = {},
    }

    feedback_unconfirmed {
        message = "feedback submission is unconfirmed",
        action = "check with the intake operator before sending the report again",
        fields = {
            detail: required(string),
        },
    }

    forward_failed {
        message = "the workspace's rift server failed the forwarded request: {source}",
        action = "check the server with `rift server start --foreground`",
        fields = {
            source: required(source),
        },
    }

    forward_unanswered {
        message = "workspace server did not answer forwarded request within {waited}",
        action = "resend the same request after a short delay",
        fields = {
            waited: required(duration),
        },
    }

    http_ports_exhausted {
        message = "every loopback port in the serving range is bound",
        action = "stop a process using a serving port, then retry",
        fields = {
            port_max: required(port),
            port_min: required(port),
        },
    }

    http_serve_failed {
        message = "HTTP MCP server failed while serving",
        action = "report this internal failure with its full context",
        fields = {
            cause: optional(cause),
            operation: required(string),
            source: optional(source),
        },
    }

    parameter_invalid {
        message = "tool arguments do not match the served schema",
        action = "correct the reported field and resend the request",
        fields = {
            accepted: optional(string),
            example: optional(string),
            field: optional(string),
            tool: required(string),
        },
    }

    project_hit_identity_missing {
        message = "project hit has no identity",
        action = "report this internal failure with its full context",
        fields = {
            hit: required(string),
        },
    }

    project_hit_identity_refused {
        message = "project hit identity was refused by ranking",
        action = "report this internal failure with its full context",
        fields = {
            cause: optional(cause),
            hit: required(string),
        },
    }

    project_hit_identity_undecodable {
        message = "project hit identity is not a qualified name",
        action = "report this internal failure with its full context",
        fields = {
            hit: required(string),
        },
    }

    project_hit_name_unmatched {
        message = "project hit name does not match requested name",
        action = "report this internal failure with its full context",
        fields = {
            hit: required(string),
        },
    }

    project_hit_unit_invalid {
        message = "project hit unit is not a source unit address",
        action = "report this internal failure with its full context",
        fields = {
            hit: required(string),
        },
    }

    proxy_identity_failed {
        message = "MCP proxy could not determine product identity",
        action = "report this internal failure with its full context",
        fields = {
            source: required(source),
        },
    }

    proxy_initialization_failed {
        message = "MCP proxy initialization failed",
        action = "resend the same request after a short delay",
        fields = {
            source: required(source),
        },
    }

    proxy_task_failed {
        message = "MCP proxy service task failed",
        action = "report this internal failure with its full context",
        fields = {
            source: required(source),
        },
    }

    proxy_unexpected_quit {
        message = "MCP service ended unexpectedly",
        action = "report this internal failure with its full context",
        fields = {},
    }

    server_identity_mismatch {
        message = "workspace server identity differs from this rift process: server pid {pid}, version {server_version}, schema digest {server_schema_digest}; this process version {version}, schema digest {schema_digest}",
        action = "run rift server stop, then retry",
        fields = {
            pid: required(pid),
            schema_digest: required(string),
            server_schema_digest: required(string),
            server_version: required(string),
            version: required(string),
        },
    }

    spawn_failed {
        message = "spawned server exited before serving",
        action = "report this internal failure with its full context",
        fields = {
            stderr: required(string, sensitive),
            stderr_truncated: optional(bool),
        },
    }

    spawn_no_output {
        message = "spawned server exited before serving and wrote no standard error",
        action = "report this internal failure with its full context",
        fields = {},
    }

    start_building {
        message = "workspace server did not finish building its first index within {waited}",
        action = "resend the same request after a short delay",
        fields = {
            waited: required(duration),
        },
    }

    start_unreachable {
        message = "no rift server answered for this workspace within {waited}; the workspace has no server this caller can reach",
        action = "starting a server is an operator action on this host",
        fields = {
            waited: required(duration),
        },
    }
}
