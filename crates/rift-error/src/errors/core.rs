use rift_error::__rift_error_definition;

__rift_error_definition!(
    configuration_command_argument_oversized,
    slug = "rift.core.configuration_command_argument_oversized",
    message = "configured command argument exceeds its byte limit",
    action = "correct the reported configuration field, then retry",
    fields = {
        bytes: optional(string),
        bytes_max: optional(string),
        field: optional(string),
    },
);

__rift_error_definition!(
    configuration_command_program_absolute,
    slug = "rift.core.configuration_command_program_absolute",
    message = "configured command executable is an absolute path",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        program: optional(string),
    },
);

__rift_error_definition!(
    configuration_command_program_dot_segment,
    slug = "rift.core.configuration_command_program_dot_segment",
    message = "configured command executable contains a dot segment",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        program: optional(string),
    },
);

__rift_error_definition!(
    configuration_command_program_empty,
    slug = "rift.core.configuration_command_program_empty",
    message = "configured command has no executable",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
    },
);

__rift_error_definition!(
    configuration_command_program_oversized,
    slug = "rift.core.configuration_command_program_oversized",
    message = "configured command executable exceeds its byte limit",
    action = "correct the reported configuration field, then retry",
    fields = {
        bytes: optional(string),
        bytes_max: optional(string),
        field: optional(string),
    },
);

__rift_error_definition!(
    configuration_command_program_whitespace,
    slug = "rift.core.configuration_command_program_whitespace",
    message = "configured command executable contains whitespace",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        program: optional(string),
    },
);

__rift_error_definition!(
    configuration_embedding_endpoint_invalid,
    slug = "rift.core.configuration_embedding_endpoint_invalid",
    message = "embedding endpoint is not an accepted URL",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        value: optional(string),
    },
);

__rift_error_definition!(
    configuration_embedding_identifier_invalid,
    slug = "rift.core.configuration_embedding_identifier_invalid",
    message = "embedding identifier value is empty or too long",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        value: optional(string),
    },
);

__rift_error_definition!(
    configuration_embedding_model_invalid,
    slug = "rift.core.configuration_embedding_model_invalid",
    message = "embedding model value does not match its configured kind",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        value: optional(string),
    },
);

__rift_error_definition!(
    configuration_file_name_invalid,
    slug = "rift.core.configuration_file_name_invalid",
    message = "excluded lockfile value is not one file name",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        name: optional(string),
    },
);

__rift_error_definition!(
    configuration_history_cpu_share_invalid,
    slug = "rift.core.configuration_history_cpu_share_invalid",
    message = "history CPU share is outside its accepted range",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        share: optional(string),
    },
);

__rift_error_definition!(
    configuration_history_release_pattern_invalid,
    slug = "rift.core.configuration_history_release_pattern_invalid",
    message = "history release pattern is invalid",
    action = "correct the reported configuration field, then retry",
    fields = {
        detail: optional(string),
        field: optional(string),
        pattern: optional(string),
    },
);

__rift_error_definition!(
    configuration_history_releases_missing,
    slug = "rift.core.configuration_history_releases_missing",
    message = "selective history strategy has no release pattern",
    action = "correct the reported configuration field, then retry",
    fields = {
        fields: optional(string),
    },
);

__rift_error_definition!(
    configuration_history_releases_outside_selective,
    slug = "rift.core.configuration_history_releases_outside_selective",
    message = "history release patterns require selective strategy",
    action = "correct the reported configuration field, then retry",
    fields = {
        fields: optional(string),
    },
);

__rift_error_definition!(
    configuration_invalid,
    slug = "rift.core.configuration_invalid",
    message = "workspace configuration failed validation",
    action = "correct the reported configuration field, then retry",
    fields = {
        detail: optional(string),
        field: optional(string),
        file: optional(string),
        first: optional(string),
        key: optional(string),
        language: optional(string),
        lsp: optional(string),
        name: optional(string),
        pattern: optional(string),
        range: optional(string),
        second: optional(string),
        value: optional(string),
        variables: optional(string),
    },
);

__rift_error_definition!(
    configuration_is_directory,
    slug = "rift.core.configuration_is_directory",
    message = "workspace configuration path is a directory",
    action = "correct the reported configuration field, then retry",
    fields = {
        detail: required(string),
        file: required(string),
        path: required(string),
    },
);

__rift_error_definition!(
    configuration_language_identity_invalid,
    slug = "rift.core.configuration_language_identity_invalid",
    message = "language table key is not a canonical language identity",
    action = "correct the reported configuration field, then retry",
    fields = {
        language: optional(string),
    },
);

__rift_error_definition!(
    configuration_language_include_duplicate,
    slug = "rift.core.configuration_language_include_duplicate",
    message = "two language entries use same include pattern",
    action = "correct the reported configuration field, then retry",
    fields = {
        first: optional(string),
        pattern: optional(string),
        second: optional(string),
    },
);

__rift_error_definition!(
    configuration_language_lsp_unknown,
    slug = "rift.core.configuration_language_lsp_unknown",
    message = "language names an LSP process that is not declared",
    action = "correct the reported configuration field, then retry",
    fields = {
        language: optional(string),
        lsp: optional(string),
    },
);

__rift_error_definition!(
    configuration_limit_out_of_range,
    slug = "rift.core.configuration_limit_out_of_range",
    message = "numeric configuration value is outside its documented range",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        range: optional(string),
        value: optional(string),
    },
);

__rift_error_definition!(
    configuration_log_capture_invalid,
    slug = "rift.core.configuration_log_capture_invalid",
    message = "logs.capture is not a tracing filter directive",
    action = "correct the reported configuration field, then retry",
    fields = {
        capture: optional(string),
        detail: optional(string),
        field: optional(string),
    },
);

__rift_error_definition!(
    configuration_lsp_embedded_extras,
    slug = "rift.core.configuration_lsp_embedded_extras",
    message = "embedded LSP engine has a spawned-process setting",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        lsp: optional(string),
    },
);

__rift_error_definition!(
    configuration_lsp_engine_missing,
    slug = "rift.core.configuration_lsp_engine_missing",
    message = "LSP table selects no engine",
    action = "correct the reported configuration field, then retry",
    fields = {
        fields: optional(string),
        lsp: optional(string),
    },
);

__rift_error_definition!(
    configuration_lsp_engine_selection_conflict,
    slug = "rift.core.configuration_lsp_engine_selection_conflict",
    message = "LSP table selects command and embedded engines",
    action = "correct the reported configuration field, then retry",
    fields = {
        fields: optional(string),
        lsp: optional(string),
    },
);

__rift_error_definition!(
    configuration_lsp_environment_key_invalid,
    slug = "rift.core.configuration_lsp_environment_key_invalid",
    message = "LSP environment key is empty or contains a forbidden character",
    action = "correct the reported configuration field, then retry",
    fields = {
        key: optional(string),
        lsp: optional(string),
    },
);

__rift_error_definition!(
    configuration_lsp_initialization_options_not_object,
    slug = "rift.core.configuration_lsp_initialization_options_not_object",
    message = "LSP initialization options are not a JSON object",
    action = "correct the reported configuration field, then retry",
    fields = {
        lsp: optional(string),
    },
);

__rift_error_definition!(
    configuration_lsp_name_invalid,
    slug = "rift.core.configuration_lsp_name_invalid",
    message = "LSP process name is not a lowercase word",
    action = "correct the reported configuration field, then retry",
    fields = {
        name: optional(string),
    },
);

__rift_error_definition!(
    configuration_malformed,
    slug = "rift.core.configuration_malformed",
    message = "workspace configuration does not match its documented shape",
    action = "correct the reported configuration field, then retry",
    fields = {
        accepted: optional(string),
        example: optional(string),
        file: required(string),
        key: optional(string),
        location: optional(string),
    },
);

__rift_error_definition!(
    configuration_oversized,
    slug = "rift.core.configuration_oversized",
    message = "workspace configuration exceeds its accepted byte limit",
    action = "correct the reported configuration field, then retry",
    fields = {
        bytes: required(unsigned),
        bytes_max: required(unsigned),
        file: required(string),
    },
);

__rift_error_definition!(
    configuration_package_registry_invalid,
    slug = "rift.core.configuration_package_registry_invalid",
    message = "dependency package registry is not a canonical endpoint",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        package: optional(string),
    },
);

__rift_error_definition!(
    configuration_package_selector_invalid,
    slug = "rift.core.configuration_package_selector_invalid",
    message = "dependency package has conflicting or missing version selector",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        package: optional(string),
    },
);

__rift_error_definition!(
    configuration_path_pattern_invalid,
    slug = "rift.core.configuration_path_pattern_invalid",
    message = "configuration path pattern breaks forward-slash path rules",
    action = "correct the reported configuration field, then retry",
    fields = {
        field: optional(string),
        pattern: optional(string),
    },
);

__rift_error_definition!(
    configuration_port_range_inverted,
    slug = "rift.core.configuration_port_range_inverted",
    message = "server port range maximum is below minimum",
    action = "correct the reported configuration field, then retry",
    fields = {
        max: optional(string),
        min: optional(string),
    },
);

__rift_error_definition!(
    configuration_port_selection_conflict,
    slug = "rift.core.configuration_port_selection_conflict",
    message = "server selects port and port range together",
    action = "correct the reported configuration field, then retry",
    fields = {
        fields: optional(string),
    },
);

__rift_error_definition!(
    configuration_search_weights_invalid,
    slug = "rift.core.configuration_search_weights_invalid",
    message = "search ranking weights are not a usable set",
    action = "correct the reported configuration field, then retry",
    fields = {
        identifier_weight: optional(string),
        lexical_weight: optional(string),
        vector_weight: optional(string),
    },
);

__rift_error_definition!(
    configuration_unit_parse,
    slug = "rift.core.configuration_unit_parse",
    message = "configuration value does not use its required unit form",
    action = "correct the reported configuration field, then retry",
    fields = {
        expected: required(string),
        value: required(string),
    },
);

__rift_error_definition!(
    configuration_unreadable,
    slug = "rift.core.configuration_unreadable",
    message = "workspace configuration could not be read",
    action = "check filesystem permissions and free space, then retry",
    fields = {
        file: required(string),
        io: required(string),
        path: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    configuration_variable_malformed,
    slug = "rift.core.configuration_variable_malformed",
    message = "configuration variable value does not match its documented shape",
    action = "correct the reported configuration field, then retry",
    fields = {
        accepted: optional(string),
        example: optional(string),
        key: required(string),
        variable: required(string),
    },
);

__rift_error_definition!(
    configuration_variable_not_unicode,
    slug = "rift.core.configuration_variable_not_unicode",
    message = "configuration variable value is not UTF-8",
    action = "correct the reported configuration field, then retry",
    fields = {
        detail: required(string),
        variable: required(string),
    },
);

__rift_error_definition!(
    configuration_variable_unknown,
    slug = "rift.core.configuration_variable_unknown",
    message = "configuration variable names no declared key",
    action = "correct the reported configuration field, then retry",
    fields = {
        accepted: required(string),
        variable: required(string),
    },
);

__rift_error_definition!(
    contribution_duplicate_fact,
    slug = "rift.core.contribution_duplicate_fact",
    message = "contribution field {field} violates rule: portable facets contain duplicates",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_invalid_kind,
    slug = "rift.core.contribution_invalid_kind",
    message = "contribution field {field} violates rule: exact kind does not use provider-kind syntax",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_invalid_language,
    slug = "rift.core.contribution_invalid_language",
    message = "contribution field {field} violates rule: language identity does not use canonical lowercase syntax",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_invalid_name,
    slug = "rift.core.contribution_invalid_name",
    message = "contribution field {field} violates rule: portable name is empty, oversized, or contains control text",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_invalid_namespace,
    slug = "rift.core.contribution_invalid_namespace",
    message = "contribution field {field} violates rule: provider-specific fact key is not reverse-domain syntax",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_invalid_namespace_version,
    slug = "rift.core.contribution_invalid_namespace_version",
    message = "contribution field {field} violates rule: provider-specific fact version is zero",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_invalid_origin,
    slug = "rift.core.contribution_invalid_origin",
    message = "contribution field {field} violates rule: source location and source kind disagree",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_invalid_record,
    slug = "rift.core.contribution_invalid_record",
    message = "contribution field {field} violates rule: normalized record state, identity, or members disagree",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_invalid_reference,
    slug = "rift.core.contribution_invalid_reference",
    message = "contribution field {field} violates rule: reference targets are empty, duplicated, or oversized",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_invalid_source_range,
    slug = "rift.core.contribution_invalid_source_range",
    message = "contribution field {field} violates rule: source range is empty or reversed",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_too_many_facts,
    slug = "rift.core.contribution_too_many_facts",
    message = "contribution field {field} violates rule: portable facts exceed their count bound",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_too_many_namespaced_facts,
    slug = "rift.core.contribution_too_many_namespaced_facts",
    message = "contribution field {field} violates rule: provider-specific facts exceed count or byte bound",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_too_much_evidence,
    slug = "rift.core.contribution_too_much_evidence",
    message = "contribution field {field} violates rule: equivalence evidence exceeds its count bound",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    contribution_unbound_identity,
    slug = "rift.core.contribution_unbound_identity",
    message = "contribution field {field} violates rule: identity anchor has no exact source binding",
    action = "correct the reported field and resend the request",
    fields = {
        field: required(string),
    },
);

__rift_error_definition!(
    identity_invalid,
    slug = "rift.core.identity_invalid",
    message = "identity is empty or contains a control character",
    action = "correct the reported field and resend the request",
    fields = {},
);

__rift_error_definition!(
    path_absolute,
    slug = "rift.core.path_absolute",
    message = "{path_kind} path is absolute",
    action = "use a workspace-relative path with `/` separators and no `.` or `..` components",
    fields = {
        path_kind: required(string),
    },
);

__rift_error_definition!(
    path_backslash,
    slug = "rift.core.path_backslash",
    message = "{path_kind} path contains a backslash",
    action = "use a workspace-relative path with `/` separators and no `.` or `..` components",
    fields = {
        path_kind: required(string),
    },
);

__rift_error_definition!(
    path_control_character,
    slug = "rift.core.path_control_character",
    message = "{path_kind} path contains a control character",
    action = "use a workspace-relative path with `/` separators and no `.` or `..` components",
    fields = {
        path_kind: required(string),
    },
);

__rift_error_definition!(
    path_dot_segment,
    slug = "rift.core.path_dot_segment",
    message = "{path_kind} path contains a dot segment",
    action = "use a workspace-relative path with `/` separators and no `.` or `..` components",
    fields = {
        path_kind: required(string),
    },
);

__rift_error_definition!(
    path_empty,
    slug = "rift.core.path_empty",
    message = "{path_kind} path is empty",
    action = "use a workspace-relative path with `/` separators and no `.` or `..` components",
    fields = {
        path_kind: required(string),
    },
);

__rift_error_definition!(
    path_empty_segment,
    slug = "rift.core.path_empty_segment",
    message = "project path contains an empty segment",
    action = "use a workspace-relative path with `/` separators and no `.` or `..` components",
    fields = {
        path_kind: required(string),
    },
);

__rift_error_definition!(
    path_non_canonical_unicode,
    slug = "rift.core.path_non_canonical_unicode",
    message = "project path does not use Unicode NFC",
    action = "use a workspace-relative path with `/` separators and no `.` or `..` components",
    fields = {
        path_kind: required(string),
    },
);

__rift_error_definition!(
    path_rift_state,
    slug = "rift.core.path_rift_state",
    message = "project path addresses Rift state",
    action = "use a workspace-relative path with `/` separators and no `.` or `..` components",
    fields = {
        path_kind: required(string),
    },
);

__rift_error_definition!(
    path_too_long,
    slug = "rift.core.path_too_long",
    message = "{path_kind} path exceeds its accepted byte limit",
    action = "use a workspace-relative path with `/` separators and no `.` or `..` components",
    fields = {
        path_kind: required(string),
    },
);

__rift_error_definition!(
    resolver_id_empty,
    slug = "rift.core.resolver_id_empty",
    message = "source resolver identity is empty",
    action = "correct the reported field and resend the request",
    fields = {
        identity: required(string),
    },
);

__rift_error_definition!(
    resolver_id_invalid_character,
    slug = "rift.core.resolver_id_invalid_character",
    message = "source resolver identity is not canonical lowercase syntax",
    action = "correct the reported field and resend the request",
    fields = {
        identity: required(string),
    },
);

__rift_error_definition!(
    resolver_id_too_long,
    slug = "rift.core.resolver_id_too_long",
    message = "source resolver identity exceeds its accepted byte limit",
    action = "correct the reported field and resend the request",
    fields = {
        identity: required(string),
    },
);

__rift_error_definition!(
    revision_zero,
    slug = "rift.core.revision_zero",
    message = "revision is zero",
    action = "correct the reported field and resend the request",
    fields = {},
);

__rift_error_definition!(
    source_unit_id_invalid_address,
    slug = "rift.core.source_unit_id_invalid_address",
    message = "source-unit identity does not use canonical Rift address structure",
    action = "correct the reported field and resend the request",
    fields = {
        identity: required(string),
    },
);

__rift_error_definition!(
    source_unit_id_invalid_encoding,
    slug = "rift.core.source_unit_id_invalid_encoding",
    message = "source-unit key has malformed percent encoding or invalid UTF-8",
    action = "correct the reported field and resend the request",
    fields = {
        identity: required(string),
    },
);

__rift_error_definition!(
    source_unit_id_invalid_key,
    slug = "rift.core.source_unit_id_invalid_key",
    message = "source-unit key breaks source-path rules: {cause}",
    action = "correct the reported field and resend the request",
    fields = {
        cause: required(cause),
        identity: required(string),
    },
);

__rift_error_definition!(
    source_unit_id_invalid_resolver,
    slug = "rift.core.source_unit_id_invalid_resolver",
    message = "source-unit resolver identity is invalid: {cause}",
    action = "correct the reported field and resend the request",
    fields = {
        cause: required(cause),
        identity: required(string),
    },
);

__rift_error_definition!(
    source_unit_id_non_canonical,
    slug = "rift.core.source_unit_id_non_canonical",
    message = "source-unit address is not in canonical form",
    action = "correct the reported field and resend the request",
    fields = {
        identity: required(string),
    },
);

__rift_error_definition!(
    source_unit_id_too_long,
    slug = "rift.core.source_unit_id_too_long",
    message = "source-unit identity exceeds its protocol byte limit",
    action = "correct the reported field and resend the request",
    fields = {
        identity: required(string),
    },
);
