use rift_error::__rift_error_definition;

__rift_error_definition!(
    read_cancelled,
    slug = "rift.server.read_cancelled",
    message = "workspace read was cancelled",
    action = "resend the request if the result is still needed",
    fields = {},
);

__rift_error_definition!(
    read_capacity_timeout,
    slug = "rift.server.read_capacity_timeout",
    message = "workspace read waited too long for blocking capacity during {operation}",
    action = "resend the same request after a short delay",
    fields = {
        operation: required(string),
        timeout_ms: required(unsigned),
    },
);

__rift_error_definition!(
    read_engine_answer,
    slug = "rift.server.read_engine_answer",
    message = "the addressed content exists but its bytes cannot be served: operation {operation}, detail {detail}",
    action = "read the request again once the engine has read the served revision",
    fields = {
        detail: required(string),
        operation: required(string),
    },
);

__rift_error_definition!(
    read_invalid,
    slug = "rift.server.read_invalid",
    message = "the request does not match the documented form: field {field}, violation {violation}",
    action = "correct the reported field and resend the request",
    fields = {
        cause: optional(cause),
        field: required(string),
        violation: required(string),
    },
);

__rift_error_definition!(
    read_not_found,
    slug = "rift.server.read_not_found",
    message = "requested source path does not exist: {path}",
    action = "search or list first, then retry with a path that answer returned",
    fields = {
        path: required(string),
    },
);

__rift_error_definition!(
    read_source_unavailable,
    slug = "rift.server.read_source_unavailable",
    message = "claimed source path is not available in the index: {path}",
    action = "request the declaration without its body, or read a source-backed unit",
    fields = {
        path: required(string),
    },
);

__rift_error_definition!(
    read_storage,
    slug = "rift.server.read_storage",
    message = "workspace file operation failed: {operation}",
    action = "check filesystem permissions and free space, then retry",
    fields = {
        io: required(string),
        operation: required(string),
        path: required(string),
    },
);

__rift_error_definition!(
    read_symbol_alternatives_unavailable,
    slug = "rift.server.read_symbol_alternatives_unavailable",
    message = "closest alternatives unavailable at the work bound {work}; required {required} work units with {remaining} remaining",
    action = "select a language, use a shorter exact name, or raise search.symbol_alternatives_work; search.symbol_alternatives_lowercase_work reserves {lowercase_work} work units per input byte",
    fields = {
        lowercase_work: required(unsigned),
        remaining: required(unsigned),
        required: required(unsigned),
        work: required(unsigned),
    },
);

__rift_error_definition!(
    read_task,
    slug = "rift.server.read_task",
    message = "workspace read task failed: {operation}",
    action = "report this internal failure with its full context",
    fields = {
        detail: required(string),
        operation: required(string),
    },
);

__rift_error_definition!(
    read_unavailable,
    slug = "rift.server.read_unavailable",
    message = "workspace index is unavailable during {operation}",
    action = "resend the same request after a short delay",
    fields = {
        detail: required(string),
        operation: required(string),
    },
);

__rift_error_definition!(
    read_unclaimed_extension,
    slug = "rift.server.read_unclaimed_extension",
    message = "no shipped syntax provider parses path extension {extension}",
    action = "address a path a shipped provider parses",
    fields = {
        extension: required(string),
    },
);

__rift_error_definition!(
    read_unsupported,
    slug = "rift.server.read_unsupported",
    message = "request uses unsupported capability {capability}",
    action = "adjust the request to a served capability, or configure a provider that serves it",
    fields = {
        capability: required(string),
    },
);

__rift_error_definition!(
    search_coverage_unavailable,
    slug = "rift.server.search_coverage_unavailable",
    message = "accepted facts do not cover {operation} in the captured selection",
    action = "supply a publication covering the requested selection, then retry",
    fields = {
        operation: required(string),
        view: optional(string),
    },
);

__rift_error_definition!(
    search_exact_release_unavailable,
    slug = "rift.server.search_exact_release_unavailable",
    message = "exact release {release} has no accepted publication",
    action = "supply an accepted publication for the exact release, then retry",
    fields = {
        operation: required(string),
        release: required(string),
    },
);

__rift_error_definition!(
    search_view_context,
    slug = "rift.server.search_view_context",
    message = "captured view {view} belongs to another workspace or scope mapping",
    action = "use the matching workspace, scope mapping, and captured view",
    fields = {
        operation: required(string),
        view: required(string),
    },
);

__rift_error_definition!(
    search_view_expired,
    slug = "rift.server.search_view_expired",
    message = "captured view {view} expired or was removed",
    action = "capture a new view and resend the request with its key",
    fields = {
        operation: required(string),
        view: required(string),
    },
);
