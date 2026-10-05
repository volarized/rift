use rift_error::__rift_error_definition;

__rift_error_definition!(
    database_failed,
    slug = "rift.index.database_failed",
    message = "index storage failed on the {database} database at {path}",
    action = "check filesystem permissions and free space below the workspace state directory, then retry",
    fields = {
        database: required(string),
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    lexical_document_location_unsupported,
    slug = "rift.index.lexical_document_location_unsupported",
    message = "lexical index cannot store this document location",
    action = "store package documents in the global index",
    fields = {
        path: optional(path),
    },
);

__rift_error_definition!(
    lexical_duplicate_identity,
    slug = "rift.index.lexical_duplicate_identity",
    message = "lexical index received a repeated document identity",
    action = "send each document identity once and retry",
    fields = {
        path: optional(path),
    },
);

__rift_error_definition!(
    lexical_record_limit,
    slug = "rift.index.lexical_record_limit",
    message = "lexical index received more records than its accepted limit of {maximum}",
    action = "reduce records below {maximum} and retry",
    fields = {
        field: required(string),
        maximum: required(unsigned),
        observed: required(unsigned),
    },
);

__rift_error_definition!(
    lexical_storage,
    slug = "rift.index.lexical_storage",
    message = "lexical index operation failed",
    action = "check filesystem permissions and free space, then retry",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    lexical_stored_kind_invalid,
    slug = "rift.index.lexical_stored_kind_invalid",
    message = "stored document kind is invalid",
    action = "repair the indexed document kind and retry",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    lexical_stored_path_invalid,
    slug = "rift.index.lexical_stored_path_invalid",
    message = "stored project path is invalid",
    action = "repair the indexed path and retry",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    lexical_unit_limit,
    slug = "rift.index.lexical_unit_limit",
    message = "lexical index received more units than its accepted limit of {maximum}",
    action = "reduce indexed units below {maximum} and retry",
    fields = {
        field: required(string),
        maximum: required(unsigned),
        observed: required(unsigned),
    },
);

__rift_error_definition!(
    lexical_unit_too_large,
    slug = "rift.index.lexical_unit_too_large",
    message = "document content exceeds its accepted byte limit of {maximum}",
    action = "shorten document content below {maximum} bytes and retry",
    fields = {
        field: required(string),
        maximum: required(unsigned),
        observed: required(unsigned),
        path: optional(path),
    },
);

__rift_error_definition!(
    workspace_cancelled,
    slug = "rift.index.workspace_cancelled",
    message = "workspace indexing was cancelled",
    action = "retry workspace indexing",
    fields = {
        path: optional(path),
    },
);

__rift_error_definition!(
    workspace_changed_during_capture,
    slug = "rift.index.workspace_changed_during_capture",
    message = "source file changed while its bytes were captured",
    action = "retry workspace indexing",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    workspace_composition,
    slug = "rift.index.workspace_composition",
    message = "workspace composition failed validation",
    action = "report this internal failure with its full context",
    fields = {
        cause: optional(cause),
    },
);

__rift_error_definition!(
    workspace_documentation_limit,
    slug = "rift.index.workspace_documentation_limit",
    message = "workspace documentation sources exceed their accepted limit of {maximum}",
    action = "reduce documentation sources below {maximum} and retry",
    fields = {
        field: required(string),
        maximum: required(unsigned),
        observed: required(unsigned),
        path: optional(path),
    },
);

__rift_error_definition!(
    workspace_file_too_large,
    slug = "rift.index.workspace_file_too_large",
    message = "source file exceeds its accepted byte limit of {maximum}",
    action = "reduce source file size below {maximum} bytes and retry",
    fields = {
        field: optional(string),
        maximum: optional(unsigned),
        observed: optional(unsigned),
        path: optional(path),
    },
);

__rift_error_definition!(
    workspace_filesystem,
    slug = "rift.index.workspace_filesystem",
    message = "workspace filesystem operation failed",
    action = "check filesystem permissions and free space, then retry",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    workspace_history,
    slug = "rift.index.workspace_history",
    message = "workspace history operation failed",
    action = "check repository state and retry",
    fields = {
        cause: optional(cause),
        path: optional(path),
    },
);

__rift_error_definition!(
    workspace_invalid_path,
    slug = "rift.index.workspace_invalid_path",
    message = "workspace path is not valid project syntax",
    action = "use a canonical project path and retry",
    fields = {
        cause: optional(cause),
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    workspace_invalid_root,
    slug = "rift.index.workspace_invalid_root",
    message = "workspace root cannot be read as a directory",
    action = "provide a readable workspace directory and retry",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    workspace_invalid_source,
    slug = "rift.index.workspace_invalid_source",
    message = "source file bytes are not valid UTF-8",
    action = "save source file as UTF-8 and retry",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    workspace_language_include_required,
    slug = "rift.index.workspace_language_include_required",
    message = "unshipped language has no nonempty include list",
    action = "add a nonempty include list for this language and retry",
    fields = {
        language: optional(string),
    },
);

__rift_error_definition!(
    workspace_language_match_conflict,
    slug = "rift.index.workspace_language_match_conflict",
    message = "workspace path matches two language entries",
    action = "make language include lists distinct and retry",
    fields = {
        language: optional(string),
        path: optional(path),
    },
);

__rift_error_definition!(
    workspace_provider,
    slug = "rift.index.workspace_provider",
    message = "provider publication failed",
    action = "report this internal failure with its full context",
    fields = {
        cause: optional(cause),
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    workspace_result_limit,
    slug = "rift.index.workspace_result_limit",
    message = "workspace search returned more results than its accepted limit of {maximum}",
    action = "reduce requested results below {maximum} and retry",
    fields = {
        field: optional(string),
        maximum: optional(unsigned),
        observed: optional(unsigned),
    },
);

__rift_error_definition!(
    workspace_syntax,
    slug = "rift.index.workspace_syntax",
    message = "Rust syntax analysis failed",
    action = "correct the source syntax and retry",
    fields = {
        cause: optional(cause),
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    workspace_too_deep,
    slug = "rift.index.workspace_too_deep",
    message = "workspace directory depth exceeds its accepted limit of {maximum}",
    action = "reduce directory depth below {maximum} and retry",
    fields = {
        field: optional(string),
        maximum: required(unsigned),
        observed: optional(unsigned),
        path: optional(path),
    },
);

__rift_error_definition!(
    workspace_too_many_files,
    slug = "rift.index.workspace_too_many_files",
    message = "workspace contains more files than its accepted limit of {maximum}",
    action = "reduce workspace files below {maximum} and retry",
    fields = {
        field: optional(string),
        maximum: optional(unsigned),
        observed: optional(unsigned),
        path: optional(path),
    },
);

__rift_error_definition!(
    workspace_workspace_too_large,
    slug = "rift.index.workspace_workspace_too_large",
    message = "workspace source bytes exceed their accepted limit of {maximum}",
    action = "reduce workspace source bytes below {maximum} and retry",
    fields = {
        field: optional(string),
        maximum: optional(unsigned),
        observed: optional(unsigned),
        path: optional(path),
    },
);

__rift_error_definition!(
    workspace_zero_limit,
    slug = "rift.index.workspace_zero_limit",
    message = "workspace index bound is zero",
    action = "set every workspace index bound above zero",
    fields = {
        cause: optional(cause),
        field: optional(string),
    },
);
