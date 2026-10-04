use rift_error::__rift_error_definition;

#[doc(hidden)]
pub const REGISTRY_NAMESPACE: &str = "rift";
#[doc(hidden)]
pub const REGISTERED_SLUGS: &[&str] = &[
    "rift.analysis.context7_entry_invalid",
    "rift.analysis.context7_malformed",
    "rift.analysis.context7_oversized",
    "rift.analysis.context7_too_many_entries",
    "rift.analysis.documentation_digest_mismatch",
    "rift.analysis.documentation_duplicate_source",
    "rift.analysis.documentation_encoding_failed",
    "rift.analysis.documentation_format_invalid",
    "rift.analysis.documentation_identity_invalid",
    "rift.analysis.documentation_limit_exceeded",
    "rift.analysis.documentation_notebook_invalid",
    "rift.analysis.documentation_order_invalid",
    "rift.analysis.documentation_origin_invalid",
    "rift.analysis.documentation_range_invalid",
    "rift.analysis.documentation_revision_invalid",
    "rift.analysis.documentation_target_missing",
    "rift.analysis.package_declarations_exceeded",
    "rift.analysis.package_identity_invalid",
    "rift.analysis.package_input_duplicate_path",
    "rift.analysis.package_input_identity_invalid",
    "rift.analysis.package_input_origin_invalid",
    "rift.analysis.package_input_too_many_bytes",
    "rift.analysis.package_input_too_many_files",
    "rift.analysis.package_provider_failed",
    "rift.analysis.package_syntax_unavailable",
    "rift.analysis.source_pattern_invalid",
    "rift.cli.install_home_unresolved",
    "rift.cli.install_remove_failed",
    "rift.cli.install_settings_unparsable",
    "rift.cli.install_template_missing_tool",
    "rift.cli.install_write_failed",
    "rift.cli.server_already_serving",
    "rift.cli.server_election_unreleased",
    "rift.cli.server_logs_unavailable",
    "rift.cli.server_spawn_failed",
    "rift.cli.server_start_exited",
    "rift.cli.server_start_timed_out",
    "rift.cli.server_stop_refused",
    "rift.cli.server_stop_request_failed",
    "rift.cli.server_stop_timed_out",
    "rift.cli.update_archive_contents_invalid",
    "rift.cli.update_archive_extraction_failed",
    "rift.cli.update_archive_file_inspection_failed",
    "rift.cli.update_archive_file_not_regular",
    "rift.cli.update_archive_file_size_invalid",
    "rift.cli.update_archive_member_too_large",
    "rift.cli.update_binary_invalid",
    "rift.cli.update_checksum_manifest_invalid",
    "rift.cli.update_checksum_mismatch",
    "rift.cli.update_checksum_read_failed",
    "rift.cli.update_download_failed",
    "rift.cli.update_download_too_large",
    "rift.cli.update_prerelease_unsupported",
    "rift.cli.update_publish_copy_failed",
    "rift.cli.update_publish_failed",
    "rift.cli.update_publish_parent_missing",
    "rift.cli.update_publish_pending_cleanup",
    "rift.cli.update_release_file_inspection_failed",
    "rift.cli.update_release_file_not_regular",
    "rift.cli.update_release_file_size_invalid",
    "rift.cli.update_release_metadata_invalid",
    "rift.cli.update_release_tag_invalid",
    "rift.cli.update_rollback_cleanup_failed",
    "rift.cli.update_rollback_failed",
    "rift.cli.update_staging_failed",
    "rift.cli.update_version_invalid",
    "rift.core.configuration_command_argument_oversized",
    "rift.core.configuration_command_program_absolute",
    "rift.core.configuration_command_program_dot_segment",
    "rift.core.configuration_command_program_empty",
    "rift.core.configuration_command_program_oversized",
    "rift.core.configuration_command_program_whitespace",
    "rift.core.configuration_embedding_endpoint_invalid",
    "rift.core.configuration_embedding_identifier_invalid",
    "rift.core.configuration_embedding_model_invalid",
    "rift.core.configuration_file_name_invalid",
    "rift.core.configuration_history_cpu_share_invalid",
    "rift.core.configuration_history_release_pattern_invalid",
    "rift.core.configuration_history_releases_missing",
    "rift.core.configuration_history_releases_outside_selective",
    "rift.core.configuration_invalid",
    "rift.core.configuration_is_directory",
    "rift.core.configuration_language_identity_invalid",
    "rift.core.configuration_language_include_duplicate",
    "rift.core.configuration_language_lsp_unknown",
    "rift.core.configuration_limit_out_of_range",
    "rift.core.configuration_log_capture_invalid",
    "rift.core.configuration_lsp_embedded_extras",
    "rift.core.configuration_lsp_engine_missing",
    "rift.core.configuration_lsp_engine_selection_conflict",
    "rift.core.configuration_lsp_environment_key_invalid",
    "rift.core.configuration_lsp_initialization_options_not_object",
    "rift.core.configuration_lsp_name_invalid",
    "rift.core.configuration_malformed",
    "rift.core.configuration_oversized",
    "rift.core.configuration_package_selector_invalid",
    "rift.core.configuration_path_pattern_invalid",
    "rift.core.configuration_port_range_inverted",
    "rift.core.configuration_port_selection_conflict",
    "rift.core.configuration_search_weights_invalid",
    "rift.core.configuration_unit_parse",
    "rift.core.configuration_unreadable",
    "rift.core.configuration_variable_malformed",
    "rift.core.configuration_variable_not_unicode",
    "rift.core.configuration_variable_unknown",
    "rift.core.contribution_duplicate_fact",
    "rift.core.contribution_invalid_kind",
    "rift.core.contribution_invalid_language",
    "rift.core.contribution_invalid_name",
    "rift.core.contribution_invalid_namespace",
    "rift.core.contribution_invalid_namespace_version",
    "rift.core.contribution_invalid_origin",
    "rift.core.contribution_invalid_record",
    "rift.core.contribution_invalid_reference",
    "rift.core.contribution_invalid_source_range",
    "rift.core.contribution_too_many_facts",
    "rift.core.contribution_too_many_namespaced_facts",
    "rift.core.contribution_too_much_evidence",
    "rift.core.contribution_unbound_identity",
    "rift.core.identity_invalid",
    "rift.core.path_absolute",
    "rift.core.path_backslash",
    "rift.core.path_control_character",
    "rift.core.path_dot_segment",
    "rift.core.path_empty",
    "rift.core.path_empty_segment",
    "rift.core.path_non_canonical_unicode",
    "rift.core.path_rift_state",
    "rift.core.path_too_long",
    "rift.core.resolver_id_empty",
    "rift.core.resolver_id_invalid_character",
    "rift.core.resolver_id_too_long",
    "rift.core.revision_zero",
    "rift.core.source_unit_id_invalid_address",
    "rift.core.source_unit_id_invalid_encoding",
    "rift.core.source_unit_id_invalid_key",
    "rift.core.source_unit_id_invalid_resolver",
    "rift.core.source_unit_id_non_canonical",
    "rift.core.source_unit_id_too_long",
    "rift.history.blob_too_large",
    "rift.history.contribution_invalid",
    "rift.history.path_unrepresentable",
    "rift.history.revision_not_commit",
    "rift.history.revision_unknown",
    "rift.history.storage",
    "rift.history.too_many_tags",
    "rift.history.tree_too_large",
    "rift.history.unversioned",
    "rift.history_store.database",
    "rift.history_store.folder",
    "rift.history_store.lock_unstable",
    "rift.index.database_failed",
    "rift.index.lexical_document_location_unsupported",
    "rift.index.lexical_duplicate_identity",
    "rift.index.lexical_record_limit",
    "rift.index.lexical_storage",
    "rift.index.lexical_stored_kind_invalid",
    "rift.index.lexical_stored_path_invalid",
    "rift.index.lexical_unit_limit",
    "rift.index.lexical_unit_too_large",
    "rift.index.workspace_cancelled",
    "rift.index.workspace_changed_during_capture",
    "rift.index.workspace_composition",
    "rift.index.workspace_documentation_limit",
    "rift.index.workspace_file_too_large",
    "rift.index.workspace_filesystem",
    "rift.index.workspace_history",
    "rift.index.workspace_invalid_path",
    "rift.index.workspace_invalid_root",
    "rift.index.workspace_invalid_source",
    "rift.index.workspace_language_include_required",
    "rift.index.workspace_language_match_conflict",
    "rift.index.workspace_provider",
    "rift.index.workspace_result_limit",
    "rift.index.workspace_syntax",
    "rift.index.workspace_too_deep",
    "rift.index.workspace_too_many_files",
    "rift.index.workspace_workspace_too_large",
    "rift.index.workspace_zero_limit",
    "rift.lsp.capabilities_position_encoding_unsupported",
    "rift.lsp.correlation_pending_requests_exceeded",
    "rift.lsp.correlation_response_unknown",
    "rift.lsp.engine_analyzing",
    "rift.lsp.engine_capability_absent",
    "rift.lsp.engine_connection_closed",
    "rift.lsp.engine_ended",
    "rift.lsp.engine_launch_failed",
    "rift.lsp.engine_message_unreadable",
    "rift.lsp.engine_program_absolute",
    "rift.lsp.engine_program_empty",
    "rift.lsp.engine_refused_retryable",
    "rift.lsp.engine_refused_terminal",
    "rift.lsp.engine_result_invalid",
    "rift.lsp.engine_timed_out",
    "rift.lsp.framing_content_length_invalid",
    "rift.lsp.framing_content_length_missing",
    "rift.lsp.framing_header_malformed",
    "rift.lsp.framing_header_too_long",
    "rift.lsp.framing_message_too_long",
    "rift.lsp.position_character_misaligned",
    "rift.lsp.position_character_out_of_range",
    "rift.lsp.position_line_out_of_range",
    "rift.lsp.position_offset_inside_line_ending",
    "rift.lsp.position_offset_misaligned",
    "rift.lsp.position_offset_out_of_range",
    "rift.lsp.uri_host_refused",
    "rift.lsp.uri_outside_root",
    "rift.lsp.uri_path_not_decodable",
    "rift.lsp.uri_root_not_absolute",
    "rift.lsp.uri_root_not_unicode",
    "rift.lsp.uri_scheme_refused",
    "rift.lsp.uri_uri_malformed",
    "rift.mcp.answer_structure_failed",
    "rift.mcp.answer_text_failed",
    "rift.mcp.answer_text_limit",
    "rift.mcp.arguments_not_object",
    "rift.mcp.election_already_serving",
    "rift.mcp.election_document_invalid",
    "rift.mcp.election_storage_failed",
    "rift.mcp.forward_unanswered",
    "rift.mcp.http_ports_exhausted",
    "rift.mcp.http_serve_failed",
    "rift.mcp.parameter_invalid",
    "rift.mcp.project_hit_identity_missing",
    "rift.mcp.project_hit_identity_refused",
    "rift.mcp.project_hit_identity_undecodable",
    "rift.mcp.project_hit_name_unmatched",
    "rift.mcp.project_hit_unit_invalid",
    "rift.mcp.proxy_identity_failed",
    "rift.mcp.proxy_initialization_failed",
    "rift.mcp.proxy_task_failed",
    "rift.mcp.proxy_unexpected_quit",
    "rift.mcp.spawn_failed",
    "rift.mcp.spawn_no_output",
    "rift.mcp.start_building",
    "rift.provider.cache_too_many_keys",
    "rift.provider.composition_dangling_input",
    "rift.provider.composition_duplicate_stage",
    "rift.provider.composition_foreign_flow",
    "rift.provider.composition_invalid_name",
    "rift.provider.composition_missing_output",
    "rift.provider.composition_stage_not_found",
    "rift.provider.composition_type_mismatch",
    "rift.provider.publication_contribution_limit",
    "rift.provider.publication_duplicate_symbol",
    "rift.provider.publication_provider_contribution_limit",
    "rift.provider.publication_provider_limit",
    "rift.provider.publication_provider_mismatch",
    "rift.provider.publication_revision_mismatch",
    "rift.provider.publication_zero_limit",
    "rift.ranking.capabilities_incompatible",
    "rift.ranking.document_field_length",
    "rift.ranking.document_identity_empty",
    "rift.ranking.fusion_constant_invalid",
    "rift.ranking.pattern_size",
    "rift.ranking.pattern_syntax",
    "rift.ranking.query_empty",
    "rift.ranking.query_length",
    "rift.ranking.query_phrase_limit",
    "rift.ranking.query_quote_unterminated",
    "rift.ranking.query_term_length",
    "rift.ranking.ranking_weights_invalid",
    "rift.ranking.reader_failed",
    "rift.search.encode_failed",
    "rift.search.model_cache_unavailable",
    "rift.search.model_configuration_invalid",
    "rift.search.model_download_failed",
    "rift.search.model_download_too_large",
    "rift.search.model_file_missing",
    "rift.search.model_source_invalid",
    "rift.search.task_failed",
    "rift.search.text_limit",
    "rift.search.tokenizer_unreadable",
    "rift.search.vector_coordinate_invalid",
    "rift.search.vector_width_mismatch",
    "rift.search.weights_unreadable",
    "rift.server.read_cancelled",
    "rift.server.read_capacity_timeout",
    "rift.server.read_engine_answer",
    "rift.server.read_invalid",
    "rift.server.read_not_found",
    "rift.server.read_source_unavailable",
    "rift.server.read_storage",
    "rift.server.read_task",
    "rift.server.read_unavailable",
    "rift.server.read_unclaimed_extension",
    "rift.server.read_unsupported",
    "rift.syntax.incompatible_grammar",
    "rift.syntax.invalid_markdown_ranges",
    "rift.syntax.invalid_query",
    "rift.syntax.markdown_progress_exceeded",
    "rift.syntax.parse_cancelled",
    "rift.syntax.position_overflow",
    "rift.syntax.source_too_large",
    "rift.syntax.too_deep",
    "rift.syntax.too_many_captures",
    "rift.syntax.too_many_markdown_inline_ranges",
    "rift.syntax.too_many_nodes",
    "rift.syntax.unknown_node_kind",
    "rift.syntax.zero_limit",
    "rift.tracing.log_batch_limit",
    "rift.tracing.log_store_failed",
];

/// Registered errors under `rift.analysis`.
pub mod analysis {
    use super::__rift_error_definition;

    __rift_error_definition!(
        context7_entry_invalid,
        slug = "rift.analysis.context7_entry_invalid",
        message = "context7.json key {key} contains an invalid entry",
        action = "correct the entry for key {key} and retry",
        fields = {
            entry: optional(string),
            file: required(string),
            key: optional(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        context7_malformed,
        slug = "rift.analysis.context7_malformed",
        message = "context7.json has an invalid JSON shape",
        action = "correct context7.json shape and retry",
        fields = {
            file: required(string),
            key: optional(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        context7_oversized,
        slug = "rift.analysis.context7_oversized",
        message = "context7.json exceeds its accepted byte limit",
        action = "reduce context7.json below its accepted byte limit and retry",
        fields = {
            file: required(string),
            key: optional(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        context7_too_many_entries,
        slug = "rift.analysis.context7_too_many_entries",
        message = "context7.json key {key} exceeds its entry limit",
        action = "reduce entries for key {key} below its accepted limit and retry",
        fields = {
            file: required(string),
            key: optional(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_digest_mismatch,
        slug = "rift.analysis.documentation_digest_mismatch",
        message = "documentation field {field} has a digest mismatch",
        action = "supply bytes matching the recorded digest and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_duplicate_source,
        slug = "rift.analysis.documentation_duplicate_source",
        message = "documentation field {field} names a duplicate source",
        action = "send each documentation source once and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_encoding_failed,
        slug = "rift.analysis.documentation_encoding_failed",
        message = "documentation field {field} could not be encoded as canonical JSON",
        action = "report this internal failure with its full context",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_format_invalid,
        slug = "rift.analysis.documentation_format_invalid",
        message = "documentation field {field} has an invalid format",
        action = "correct documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_identity_invalid,
        slug = "rift.analysis.documentation_identity_invalid",
        message = "documentation field {field} has an invalid identity",
        action = "correct documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_limit_exceeded,
        slug = "rift.analysis.documentation_limit_exceeded",
        message = "documentation field {field} exceeds its accepted limit",
        action = "reduce documentation field {field} below its accepted limit and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_notebook_invalid,
        slug = "rift.analysis.documentation_notebook_invalid",
        message = "documentation field {field} has an invalid notebook shape",
        action = "correct notebook field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_order_invalid,
        slug = "rift.analysis.documentation_order_invalid",
        message = "documentation field {field} is not in required order",
        action = "order documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_origin_invalid,
        slug = "rift.analysis.documentation_origin_invalid",
        message = "documentation field {field} conflicts with its source address",
        action = "correct documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_range_invalid,
        slug = "rift.analysis.documentation_range_invalid",
        message = "documentation field {field} has a range outside source bytes",
        action = "correct documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_revision_invalid,
        slug = "rift.analysis.documentation_revision_invalid",
        message = "documentation field {field} has an unsupported publication revision",
        action = "use the supported publication revision and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        documentation_target_missing,
        slug = "rift.analysis.documentation_target_missing",
        message = "documentation field {field} names a missing target",
        action = "supply an existing target for documentation field {field}",
        fields = {
            field: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        package_declarations_exceeded,
        slug = "rift.analysis.package_declarations_exceeded",
        message = "package declaration count exceeds its accepted limit",
        action = "reduce package declarations below the accepted limit and retry",
        fields = {
            package: required(string),
            path: optional(path),
        },
    );

    __rift_error_definition!(
        package_identity_invalid,
        slug = "rift.analysis.package_identity_invalid",
        message = "package identity cannot form a source unit, resolver, or symbol",
        action = "correct package identity and retry",
        fields = {
            cause: optional(cause),
            package: required(string),
            path: optional(path),
        },
    );

    __rift_error_definition!(
        package_input_duplicate_path,
        slug = "rift.analysis.package_input_duplicate_path",
        message = "package source path appears more than once",
        action = "send each package source path once and retry",
        fields = {
            path: optional(path),
        },
    );

    __rift_error_definition!(
        package_input_identity_invalid,
        slug = "rift.analysis.package_input_identity_invalid",
        message = "package identity cannot form a source unit",
        action = "correct package identity and retry",
        fields = {
            cause: optional(cause),
            path: optional(path),
        },
    );

    __rift_error_definition!(
        package_input_origin_invalid,
        slug = "rift.analysis.package_input_origin_invalid",
        message = "package source origin does not identify this package",
        action = "correct package source origin and retry",
        fields = {
            path: optional(path),
        },
    );

    __rift_error_definition!(
        package_input_too_many_bytes,
        slug = "rift.analysis.package_input_too_many_bytes",
        message = "package source bytes exceed their accepted limit",
        action = "reduce package source bytes below its accepted limit and retry",
        fields = {
            bound: required(unsigned),
            field: required(string),
            observed: required(unsigned),
        },
    );

    __rift_error_definition!(
        package_input_too_many_files,
        slug = "rift.analysis.package_input_too_many_files",
        message = "package source count exceeds its accepted limit",
        action = "reduce package source count below its accepted limit and retry",
        fields = {
            bound: required(unsigned),
            field: required(string),
            observed: required(unsigned),
        },
    );

    __rift_error_definition!(
        package_provider_failed,
        slug = "rift.analysis.package_provider_failed",
        message = "package semantic publication failed",
        action = "report this internal failure with its full context",
        fields = {
            cause: optional(cause),
            package: required(string),
            path: optional(path),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        package_syntax_unavailable,
        slug = "rift.analysis.package_syntax_unavailable",
        message = "no shipped syntax provider accepts package file extension",
        action = "use a supported package file extension and retry",
        fields = {
            package: required(string),
            path: required(path),
        },
    );

    __rift_error_definition!(
        source_pattern_invalid,
        slug = "rift.analysis.source_pattern_invalid",
        message = "source path pattern is invalid",
        action = "correct the source path pattern and retry",
        fields = {
            pattern: optional(string),
            source: required(source),
        },
    );
}

/// Registered errors under `rift.cli`.
pub mod cli {
    use super::__rift_error_definition;

    __rift_error_definition!(
        install_home_unresolved,
        slug = "rift.cli.install_home_unresolved",
        message = "the operator's home directory could not be resolved",
        action = "set HOME (or USERPROFILE on Windows) and retry `rift install claude --user`",
        fields = {
            checked: required(string),
        },
    );

    __rift_error_definition!(
        install_remove_failed,
        slug = "rift.cli.install_remove_failed",
        message = "the generated Claude Code skill could not be removed: {path}",
        action = "ensure the target directory is writable and retry `rift install claude --remove`",
        fields = {
            path: required(path),
            source: required(source),
        },
    );

    __rift_error_definition!(
        install_settings_unparsable,
        slug = "rift.cli.install_settings_unparsable",
        message = "the target settings.json could not be read as a JSON hook document: {path}",
        action = "fix or remove the file, then run the same `rift install` command again",
        fields = {
            path: required(path),
            source: required(source),
        },
    );

    __rift_error_definition!(
        install_template_missing_tool,
        slug = "rift.cli.install_template_missing_tool",
        message = "the generated Claude Code skill names a tool the served MCP surface does not have: {tool}",
        action = "rebuild rift so the binary and its served tool surface match, then retry `rift install claude`",
        fields = {
            tool: required(string),
        },
    );

    __rift_error_definition!(
        install_write_failed,
        slug = "rift.cli.install_write_failed",
        message = "the generated Claude Code skill could not be written: {path}",
        action = "ensure the target directory is writable and retry `rift install claude`",
        fields = {
            path: required(path),
            source: required(source),
        },
    );

    __rift_error_definition!(
        server_already_serving,
        slug = "rift.cli.server_already_serving",
        message = "another rift server already serves this workspace",
        action = "connect to the listed server, or run `rift server stop` before serving again",
        fields = {
            detail: optional(string),
            listening: optional(string),
            pid: optional(pid),
        },
    );

    __rift_error_definition!(
        server_election_unreleased,
        slug = "rift.cli.server_election_unreleased",
        message = "server stopped answering its port but still holds the election: process {pid}, waited {waited}",
        action = "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
        fields = {
            pid: required(pid),
            waited: required(duration),
        },
    );

    __rift_error_definition!(
        server_logs_unavailable,
        slug = "rift.cli.server_logs_unavailable",
        message = "the workspace's recorded server logs could not be read",
        action = "ensure no other process holds `.rift/metrics` exclusively and retry",
        fields = {
            detail: optional(string),
            operation: optional(string),
            source: optional(cause),
        },
    );

    __rift_error_definition!(
        server_spawn_failed,
        slug = "rift.cli.server_spawn_failed",
        message = "the rift server process could not be started: {source}",
        action = "check that the rift binary is runnable, or run `rift server start --foreground` to serve in this process",
        fields = {
            operation: required(string),
            source: required(source),
        },
    );

    __rift_error_definition!(
        server_start_exited,
        slug = "rift.cli.server_start_exited",
        message = "the spawned rift server exited before publishing its lock document: process {pid}",
        action = "read `.rift/server.stderr`, or run `rift server logs --level error`, for what the server reported before it exited",
        fields = {
            pid: required(pid),
        },
    );

    __rift_error_definition!(
        server_start_timed_out,
        slug = "rift.cli.server_start_timed_out",
        message = "the started rift server did not report serving within {waited}",
        action = "run `rift server start --foreground` to see the server's diagnostics on stderr",
        fields = {
            waited: required(duration),
        },
    );

    __rift_error_definition!(
        server_stop_refused,
        slug = "rift.cli.server_stop_refused",
        message = "the server refused the stop request with status {status}",
        action = "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
        fields = {
            detail: optional(string),
            status: required(unsigned),
        },
    );

    __rift_error_definition!(
        server_stop_request_failed,
        slug = "rift.cli.server_stop_request_failed",
        message = "the server stop request could not be delivered: {source}",
        action = "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
        fields = {
            operation: required(string),
            source: required(source),
        },
    );

    __rift_error_definition!(
        server_stop_timed_out,
        slug = "rift.cli.server_stop_timed_out",
        message = "the server accepted the stop request but kept serving after {waited}",
        action = "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
        fields = {
            detail: optional(string),
            listening: optional(string),
            pid: optional(pid),
            waited: required(duration),
        },
    );

    __rift_error_definition!(
        update_archive_contents_invalid,
        slug = "rift.cli.update_archive_contents_invalid",
        message = "release archive contents are invalid: expected exactly one binary, README.md, and LICENSE.md member; retry `rift update`",
        action = "retry `rift update`",
        fields = {},
    );

    __rift_error_definition!(
        update_archive_extraction_failed,
        slug = "rift.cli.update_archive_extraction_failed",
        message = "release archive could not be extracted: retry `rift update`; if this persists the download may be corrupted",
        action = "retry `rift update`; if this persists the download may be corrupted",
        fields = {
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_archive_file_inspection_failed,
        slug = "rift.cli.update_archive_file_inspection_failed",
        message = "downloaded release file at `{path}` could not be inspected: {source}: retry `rift update`",
        action = "retry `rift update`",
        fields = {
            path: required(path),
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_archive_file_not_regular,
        slug = "rift.cli.update_archive_file_not_regular",
        message = "downloaded release file at `{path}` is not a regular file: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
        action = "retry `rift update` or create an issue",
        fields = {
            path: required(path),
        },
    );

    __rift_error_definition!(
        update_archive_file_size_invalid,
        slug = "rift.cli.update_archive_file_size_invalid",
        message = "downloaded release file at `{path}` has incorrect size of {size} bytes, expected between 1 and {bytes_max} bytes: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
        action = "retry `rift update` or create an issue",
        fields = {
            bytes_max: required(unsigned),
            path: required(path),
            size: required(unsigned),
        },
    );

    __rift_error_definition!(
        update_archive_member_too_large,
        slug = "rift.cli.update_archive_member_too_large",
        message = "release archive member is empty or exceeds {bytes_max} bytes: retry `rift update`; if this persists the release may be malformed",
        action = "retry `rift update`; if this persists the release may be malformed",
        fields = {
            bytes_max: required(unsigned),
        },
    );

    __rift_error_definition!(
        update_binary_invalid,
        slug = "rift.cli.update_binary_invalid",
        message = "current Rift executable (invoked as `{invoked_as}`) could not be located: {source}: reinstall Rift if the binary was moved or deleted",
        action = "reinstall Rift if the binary was moved or deleted",
        fields = {
            invoked_as: required(string),
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_checksum_manifest_invalid,
        slug = "rift.cli.update_checksum_manifest_invalid",
        message = "release checksum manifest is invalid: expected `sha256sum`-format lines naming the release archive exactly once; retry `rift update`",
        action = "retry `rift update`",
        fields = {},
    );

    __rift_error_definition!(
        update_checksum_mismatch,
        slug = "rift.cli.update_checksum_mismatch",
        message = "the downloaded release does not match its published checksum: expected {expected}, actual {actual}; retry `rift update`, and raise an issue at https://github.com/volarized/rift/issues if the mismatch repeats",
        action = "retry `rift update` and raise an issue if mismatch repeats",
        fields = {
            actual: required(string),
            expected: required(string),
        },
    );

    __rift_error_definition!(
        update_checksum_read_failed,
        slug = "rift.cli.update_checksum_read_failed",
        message = "release checksum could not be verified: retry `rift update`",
        action = "retry `rift update`",
        fields = {
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_download_failed,
        slug = "rift.cli.update_download_failed",
        message = "release download failed: check network access to github.com and retry `rift update`",
        action = "check network access to github.com and retry `rift update`",
        fields = {
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_download_too_large,
        slug = "rift.cli.update_download_too_large",
        message = "release download was empty or exceeded {bytes_max} bytes: retry `rift update`; if this persists the release assets may be malformed",
        action = "retry `rift update`; if this persists the release assets may be malformed",
        fields = {
            bytes_max: required(unsigned),
        },
    );

    __rift_error_definition!(
        update_prerelease_unsupported,
        slug = "rift.cli.update_prerelease_unsupported",
        message = "release tag `{tag}` is a pre-release or build: only stable releases of the form `vMAJOR.MINOR.PATCH` are supported",
        action = "use a stable release tag of the form `vMAJOR.MINOR.PATCH`",
        fields = {
            tag: required(string),
        },
    );

    __rift_error_definition!(
        update_publish_copy_failed,
        slug = "rift.cli.update_publish_copy_failed",
        message = "Rift update could not be published: copying the downloaded binary into `{path}` failed: ensure the directory is writable and has free space, then retry `rift update`",
        action = "ensure the directory is writable and has free space, then retry `rift update`",
        fields = {
            cause: required(cause),
            path: required(path),
        },
    );

    __rift_error_definition!(
        update_publish_failed,
        slug = "rift.cli.update_publish_failed",
        message = "Rift update could not be published: {operation} `{path}` failed: {source}: ensure the directory is writable and retry `rift update`",
        action = "ensure the directory is writable and retry `rift update`",
        fields = {
            operation: required(string),
            path: required(path),
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_publish_parent_missing,
        slug = "rift.cli.update_publish_parent_missing",
        message = "current executable `{path}` has no parent directory: install Rift in a regular directory before updating",
        action = "install Rift in a regular directory before updating",
        fields = {
            path: required(path),
        },
    );

    __rift_error_definition!(
        update_publish_pending_cleanup,
        slug = "rift.cli.update_publish_pending_cleanup",
        message = "another Rift update is pending cleanup: retry after the previous Rift process exits, or delete `{path}`",
        action = "retry after the previous Rift process exits, or delete the pending file",
        fields = {
            path: required(path),
        },
    );

    __rift_error_definition!(
        update_release_file_inspection_failed,
        slug = "rift.cli.update_release_file_inspection_failed",
        message = "downloaded release file at `{path}` could not be inspected: {source}: retry `rift update`",
        action = "retry `rift update`",
        fields = {
            path: required(path),
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_release_file_not_regular,
        slug = "rift.cli.update_release_file_not_regular",
        message = "downloaded release file at `{path}` is not a regular file: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
        action = "retry `rift update` or create an issue",
        fields = {
            path: required(path),
        },
    );

    __rift_error_definition!(
        update_release_file_size_invalid,
        slug = "rift.cli.update_release_file_size_invalid",
        message = "downloaded release file at `{path}` has incorrect size of {size} bytes, expected between 1 and {bytes_max} bytes: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
        action = "retry `rift update` or create an issue",
        fields = {
            bytes_max: required(unsigned),
            path: required(path),
            size: required(unsigned),
        },
    );

    __rift_error_definition!(
        update_release_metadata_invalid,
        slug = "rift.cli.update_release_metadata_invalid",
        message = "latest release metadata is invalid: retry `rift update` or check https://github.com/volarized/rift/releases",
        action = "retry `rift update` or check the release page",
        fields = {
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_release_tag_invalid,
        slug = "rift.cli.update_release_tag_invalid",
        message = "release tag `{tag}` is invalid: expected the form `vMAJOR.MINOR.PATCH`, such as `v0.0.2`",
        action = "use a stable release tag of the form `vMAJOR.MINOR.PATCH`",
        fields = {
            source: optional(source),
            tag: required(string),
        },
    );

    __rift_error_definition!(
        update_rollback_cleanup_failed,
        slug = "rift.cli.update_rollback_cleanup_failed",
        message = "We were not able to clean up the old binary at `{path}`: {source}: delete the file manually",
        action = "delete the file manually",
        fields = {
            path: required(path),
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_rollback_failed,
        slug = "rift.cli.update_rollback_failed",
        message = "Rift update publish and rollback of `{path}` both failed: reinstall Rift from an official release",
        action = "reinstall Rift from an official release",
        fields = {
            path: required(path),
            source: required(source),
        },
    );

    __rift_error_definition!(
        update_staging_failed,
        slug = "rift.cli.update_staging_failed",
        message = "update staging directory could not be created under `{path}` ({space}): {source}: ensure the directory is writable and has free space, then retry `rift update`",
        action = "ensure the directory is writable and has free space, then retry `rift update`",
        fields = {
            path: required(path),
            source: required(source),
            space: required(string),
        },
    );

    __rift_error_definition!(
        update_version_invalid,
        slug = "rift.cli.update_version_invalid",
        message = "installed Rift version `{raw}` at `{path}` is invalid: {source}: reinstall Rift from an official release",
        action = "reinstall Rift from an official release",
        fields = {
            path: required(path),
            raw: required(string),
            source: required(source),
        },
    );
}

/// Registered errors under `rift.core`.
pub mod core {
    use super::__rift_error_definition;

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
}

/// Registered errors under `rift.history`.
pub mod history {
    use super::__rift_error_definition;

    __rift_error_definition!(
        blob_too_large,
        slug = "rift.history.blob_too_large",
        message = "committed file {path} has {size} bytes, above accepted limit {bytes_max}",
        action = "use a revision with a smaller file",
        fields = {
            bytes_max: required(unsigned),
            path: required(path),
            size: required(unsigned),
        },
    );

    __rift_error_definition!(
        contribution_invalid,
        slug = "rift.history.contribution_invalid",
        message = "history Contribution conversion rejected: {detail}",
        action = "check the history Contribution data and retry",
        fields = {
            detail: required(string),
        },
    );

    __rift_error_definition!(
        path_unrepresentable,
        slug = "rift.history.path_unrepresentable",
        message = "committed path cannot be represented as UTF-8: {path}",
        action = "rename the committed path to valid UTF-8 and retry",
        fields = {
            path: required(string),
        },
    );

    __rift_error_definition!(
        revision_not_commit,
        slug = "rift.history.revision_not_commit",
        message = "revision {rev} resolves to {resolved_kind}, not a commit",
        action = "use a revision that names a commit",
        fields = {
            requires: required(string),
            resolved_kind: required(string),
            rev: required(string),
        },
    );

    __rift_error_definition!(
        revision_unknown,
        slug = "rift.history.revision_unknown",
        message = "revision does not resolve: {rev}",
        action = "use a branch, tag, or commit id this repository resolves",
        fields = {
            requires: required(string),
            rev: required(string),
        },
    );

    __rift_error_definition!(
        storage,
        slug = "rift.history.storage",
        message = "repository storage failed during {operation}: {detail}",
        action = "check repository storage and retry",
        fields = {
            detail: required(string),
            operation: required(string),
        },
    );

    __rift_error_definition!(
        too_many_tags,
        slug = "rift.history.too_many_tags",
        message = "repository tag count exceeds accepted limit {tags_max}",
        action = "reduce repository tags or raise the tag limit",
        fields = {
            limit: required(string),
            tags_max: required(unsigned),
        },
    );

    __rift_error_definition!(
        tree_too_large,
        slug = "rift.history.tree_too_large",
        message = "revision tree exceeds accepted entry limit {entries_max}",
        action = "use a revision with a smaller tree",
        fields = {
            entries_max: required(unsigned),
            limit: required(string),
        },
    );

    __rift_error_definition!(
        unversioned,
        slug = "rift.history.unversioned",
        message = "workspace has no git repository: {workspace}",
        action = "run `git init`, or omit `rev` to read current tree",
        fields = {
            requires: required(string),
            workspace: required(path),
        },
    );
}

/// Registered errors under `rift.history_store`.
pub mod history_store {
    use super::__rift_error_definition;

    __rift_error_definition!(
        database,
        slug = "rift.history_store.database",
        message = "history store database operation failed: {operation}: {detail}",
        action = "check the history store database and retry",
        fields = {
            detail: required(source),
            operation: required(string),
        },
    );

    __rift_error_definition!(
        folder,
        slug = "rift.history_store.folder",
        message = "history store folder operation failed: {operation} {path}: {detail}",
        action = "check the history store path and permissions",
        fields = {
            detail: required(source),
            operation: required(string),
            path: required(path),
        },
    );

    __rift_error_definition!(
        lock_unstable,
        slug = "rift.history_store.lock_unstable",
        message = "history store live lock changed during {attempts} attempts: {path}",
        action = "retry after concurrent history store cleanup finishes",
        fields = {
            attempts: required(unsigned),
            path: required(path),
        },
    );
}

/// Registered errors under `rift.index`.
pub mod index {
    use super::__rift_error_definition;

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
}

/// Registered errors under `rift.lsp`.
pub mod lsp {
    use super::__rift_error_definition;

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
}

/// Registered errors under `rift.mcp`.
pub mod mcp {
    use super::__rift_error_definition;

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
}

/// Registered errors under `rift.provider`.
pub mod provider {
    use super::__rift_error_definition;

    __rift_error_definition!(
        cache_too_many_keys,
        slug = "rift.provider.cache_too_many_keys",
        message = "cache key count exceeds its configured limit",
        action = "reduce cache keys to its configured limit and retry",
        fields = {},
    );

    __rift_error_definition!(
        composition_dangling_input,
        slug = "rift.provider.composition_dangling_input",
        message = "composition stage still has a consumer",
        action = "remove consumers before removing stage and retry",
        fields = {
            stage: required(string),
        },
    );

    __rift_error_definition!(
        composition_duplicate_stage,
        slug = "rift.provider.composition_duplicate_stage",
        message = "composition stage path already exists",
        action = "use a unique stage path and retry",
        fields = {
            stage: required(string),
        },
    );

    __rift_error_definition!(
        composition_foreign_flow,
        slug = "rift.provider.composition_foreign_flow",
        message = "composition flow belongs to another builder",
        action = "use flow handles from this builder and retry",
        fields = {},
    );

    __rift_error_definition!(
        composition_invalid_name,
        slug = "rift.provider.composition_invalid_name",
        message = "composition stage name is invalid",
        action = "use a canonical stage name and retry",
        fields = {},
    );

    __rift_error_definition!(
        composition_missing_output,
        slug = "rift.provider.composition_missing_output",
        message = "composition has no selected output",
        action = "select one output stage and retry",
        fields = {},
    );

    __rift_error_definition!(
        composition_stage_not_found,
        slug = "rift.provider.composition_stage_not_found",
        message = "composition stage does not exist",
        action = "use an existing stage path and retry",
        fields = {
            stage: required(string),
        },
    );

    __rift_error_definition!(
        composition_type_mismatch,
        slug = "rift.provider.composition_type_mismatch",
        message = "composition stage input and output types do not match",
        action = "connect stages with matching types and retry",
        fields = {
            stage: required(string),
        },
    );

    __rift_error_definition!(
        publication_contribution_limit,
        slug = "rift.provider.publication_contribution_limit",
        message = "total contribution count exceeds its configured limit",
        action = "reduce total contributions to configured limit and retry",
        fields = {
            field: required(string),
        },
    );

    __rift_error_definition!(
        publication_duplicate_symbol,
        slug = "rift.provider.publication_duplicate_symbol",
        message = "provider publication repeats a provider symbol",
        action = "publish each provider symbol once and retry",
        fields = {
            field: required(string),
        },
    );

    __rift_error_definition!(
        publication_provider_contribution_limit,
        slug = "rift.provider.publication_provider_contribution_limit",
        message = "provider contribution count exceeds its configured limit",
        action = "reduce provider contributions to configured limit and retry",
        fields = {
            field: required(string),
        },
    );

    __rift_error_definition!(
        publication_provider_limit,
        slug = "rift.provider.publication_provider_limit",
        message = "provider publication count exceeds its configured limit",
        action = "reduce provider publications to configured limit and retry",
        fields = {
            field: required(string),
        },
    );

    __rift_error_definition!(
        publication_provider_mismatch,
        slug = "rift.provider.publication_provider_mismatch",
        message = "contribution provider does not match publication provider",
        action = "publish contributions under matching provider identity and retry",
        fields = {
            field: required(string),
        },
    );

    __rift_error_definition!(
        publication_revision_mismatch,
        slug = "rift.provider.publication_revision_mismatch",
        message = "contribution revision does not match publication revision",
        action = "publish contributions under matching revision and retry",
        fields = {
            field: required(string),
        },
    );

    __rift_error_definition!(
        publication_zero_limit,
        slug = "rift.provider.publication_zero_limit",
        message = "provider publication limit is zero",
        action = "set each provider publication limit above zero and retry",
        fields = {
            field: required(string),
        },
    );
}

/// Registered errors under `rift.ranking`.
pub mod ranking {
    use super::__rift_error_definition;

    __rift_error_definition!(
        capabilities_incompatible,
        slug = "rift.ranking.capabilities_incompatible",
        message = "index capabilities cannot be combined for ranking",
        action = "report this internal failure with its full context",
        fields = {},
    );

    __rift_error_definition!(
        document_field_length,
        slug = "rift.ranking.document_field_length",
        message = "document field {subject} exceeds its accepted byte limit of {limit} bytes",
        action = "report this internal failure with its full context",
        fields = {
            field: required(string),
            limit: required(unsigned),
            required: required(unsigned),
            subject: required(string),
        },
    );

    __rift_error_definition!(
        document_identity_empty,
        slug = "rift.ranking.document_identity_empty",
        message = "document identity is empty",
        action = "report this internal failure with its full context",
        fields = {
            subject: optional(string),
        },
    );

    __rift_error_definition!(
        fusion_constant_invalid,
        slug = "rift.ranking.fusion_constant_invalid",
        message = "ranking constant is outside its accepted range",
        action = "set the ranking constant within its accepted range, then retry",
        fields = {
            subject: required(string),
        },
    );

    __rift_error_definition!(
        pattern_size,
        slug = "rift.ranking.pattern_size",
        message = "compiled pattern exceeds its accepted size: {subject}",
        action = "reduce the compiled pattern size and retry",
        fields = {
            subject: required(string),
        },
    );

    __rift_error_definition!(
        pattern_syntax,
        slug = "rift.ranking.pattern_syntax",
        message = "pattern syntax is invalid: {subject}",
        action = "correct the pattern syntax and retry",
        fields = {
            subject: required(string),
        },
    );

    __rift_error_definition!(
        query_empty,
        slug = "rift.ranking.query_empty",
        message = "query is empty",
        action = "provide query text and resend the request",
        fields = {
            subject: optional(string),
        },
    );

    __rift_error_definition!(
        query_length,
        slug = "rift.ranking.query_length",
        message = "query exceeds its accepted byte limit of {limit} bytes",
        action = "shorten the query below {limit} bytes and resend the request",
        fields = {
            field: required(string),
            limit: required(unsigned),
            required: required(unsigned),
            subject: required(string),
        },
    );

    __rift_error_definition!(
        query_phrase_limit,
        slug = "rift.ranking.query_phrase_limit",
        message = "query contains more quoted phrases than the accepted limit of {limit}",
        action = "reduce quoted phrases to {limit} and resend the request",
        fields = {
            field: required(string),
            limit: required(unsigned),
            required: required(unsigned),
            subject: required(string),
        },
    );

    __rift_error_definition!(
        query_quote_unterminated,
        slug = "rift.ranking.query_quote_unterminated",
        message = "query contains an unclosed quote",
        action = "close every quoted phrase and resend the request",
        fields = {
            subject: optional(string),
        },
    );

    __rift_error_definition!(
        query_term_length,
        slug = "rift.ranking.query_term_length",
        message = "query term exceeds its accepted byte limit of {limit} bytes",
        action = "shorten the query term below {limit} bytes and resend the request",
        fields = {
            field: required(string),
            limit: required(unsigned),
            required: required(unsigned),
            subject: required(string),
        },
    );

    __rift_error_definition!(
        ranking_weights_invalid,
        slug = "rift.ranking.ranking_weights_invalid",
        message = "ranking shares must be finite values from zero to one with a positive sum",
        action = "set finite ranking shares from zero to one with a positive sum, then retry",
        fields = {
            subject: required(string),
        },
    );

    __rift_error_definition!(
        reader_failed,
        slug = "rift.ranking.reader_failed",
        message = "search reader failed",
        action = "check filesystem permissions and free space, then retry",
        fields = {
            source: required(source),
            subject: optional(string),
        },
    );
}

/// Registered errors under `rift.search`.
pub mod search {
    use super::__rift_error_definition;

    __rift_error_definition!(
        encode_failed,
        slug = "rift.search.encode_failed",
        message = "text encoding failed",
        action = "check model input and retry",
        fields = {
            source: optional(source),
            stage: optional(string),
        },
    );

    __rift_error_definition!(
        model_cache_unavailable,
        slug = "rift.search.model_cache_unavailable",
        message = "model cache directory could not be resolved from environment variables {variables}",
        action = "set a model cache directory and retry",
        fields = {
            variables: required(string),
        },
    );

    __rift_error_definition!(
        model_configuration_invalid,
        slug = "rift.search.model_configuration_invalid",
        message = "model configuration is invalid",
        action = "provide a model configuration this encoder serves and retry",
        fields = {
            path: optional(path),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        model_download_failed,
        slug = "rift.search.model_download_failed",
        message = "model file download failed: {subject}",
        action = "check network access and retry",
        fields = {
            source: optional(source),
            subject: required(string),
        },
    );

    __rift_error_definition!(
        model_download_too_large,
        slug = "rift.search.model_download_too_large",
        message = "model file body at {url} is empty or exceeds accepted byte limit {bytes_max}",
        action = "provide a nonempty model file within its byte limit and retry",
        fields = {
            bytes_max: required(unsigned),
            url: required(string),
        },
    );

    __rift_error_definition!(
        model_file_missing,
        slug = "rift.search.model_file_missing",
        message = "model directory is missing file {subject}",
        action = "supply the missing model file and retry",
        fields = {
            subject: required(path),
        },
    );

    __rift_error_definition!(
        model_source_invalid,
        slug = "rift.search.model_source_invalid",
        message = "model source {model} has invalid form; expected {expected}",
        action = "use a model source in the expected form and retry",
        fields = {
            expected: required(string),
            model: required(string),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        task_failed,
        slug = "rift.search.task_failed",
        message = "search task did not return",
        action = "retry search",
        fields = {
            source: optional(source),
        },
    );

    __rift_error_definition!(
        text_limit,
        slug = "rift.search.text_limit",
        message = "encoder received {observed} texts, exceeding accepted limit {limit}",
        action = "reduce input texts below {limit} and retry",
        fields = {
            limit: required(unsigned),
            observed: required(unsigned),
        },
    );

    __rift_error_definition!(
        tokenizer_unreadable,
        slug = "rift.search.tokenizer_unreadable",
        message = "model tokenizer is unreadable",
        action = "repair the model tokenizer file and retry",
        fields = {
            path: optional(path),
            source: optional(source),
        },
    );

    __rift_error_definition!(
        vector_coordinate_invalid,
        slug = "rift.search.vector_coordinate_invalid",
        message = "embedded coordinate {coordinate} cannot be represented in the stored f32 format",
        action = "use an embedding model that returns finite coordinates within the stored range",
        fields = {
            coordinate: required(string),
        },
    );

    __rift_error_definition!(
        vector_width_mismatch,
        slug = "rift.search.vector_width_mismatch",
        message = "query vector width {query_width} does not match stored vector width {stored_width}",
        action = "use vectors with the stored width and retry",
        fields = {
            query_width: required(unsigned),
            stored_width: required(unsigned),
        },
    );

    __rift_error_definition!(
        weights_unreadable,
        slug = "rift.search.weights_unreadable",
        message = "model weights are unreadable",
        action = "repair the model weights file and retry",
        fields = {
            path: optional(path),
            source: optional(source),
        },
    );
}

/// Registered errors under `rift.server`.
pub mod server {
    use super::__rift_error_definition;

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
}

/// Registered errors under `rift.syntax`.
pub mod syntax {
    use super::__rift_error_definition;

    __rift_error_definition!(
        incompatible_grammar,
        slug = "rift.syntax.incompatible_grammar",
        message = "grammar ABI {grammar_abi_version} is outside runtime range {runtime_abi_min} to {runtime_abi_max}",
        action = "use a grammar built for the accepted runtime ABI range",
        fields = {
            grammar_abi_version: required(unsigned),
            runtime_abi_max: required(unsigned),
            runtime_abi_min: required(unsigned),
        },
    );

    __rift_error_definition!(
        invalid_markdown_ranges,
        slug = "rift.syntax.invalid_markdown_ranges",
        message = "Tree-sitter rejected Markdown inline ranges",
        action = "report this internal failure with its full context",
        fields = {
            path: required(path),
        },
    );

    __rift_error_definition!(
        invalid_query,
        slug = "rift.syntax.invalid_query",
        message = "syntax query is invalid at line {line_number}: {line_text}",
        action = "correct the query line and retry",
        fields = {
            line_number: required(unsigned),
            line_text: required(string),
            source: required(source),
        },
    );

    __rift_error_definition!(
        markdown_progress_exceeded,
        slug = "rift.syntax.markdown_progress_exceeded",
        message = "Markdown parser exceeded progress callback limit {progress_callbacks_max}",
        action = "reduce Markdown source size and retry",
        fields = {
            path: required(path),
            progress_callbacks_max: required(unsigned),
        },
    );

    __rift_error_definition!(
        parse_cancelled,
        slug = "rift.syntax.parse_cancelled",
        message = "syntax parser returned no tree",
        action = "retry syntax parsing",
        fields = {
            path: optional(path),
        },
    );

    __rift_error_definition!(
        position_overflow,
        slug = "rift.syntax.position_overflow",
        message = "syntax node position does not fit the accepted byte width",
        action = "report this internal failure with its full context",
        fields = {
            end_byte: required(unsigned),
            node_kind: required(string),
            source: required(source),
            start_byte: required(unsigned),
        },
    );

    __rift_error_definition!(
        source_too_large,
        slug = "rift.syntax.source_too_large",
        message = "source bytes {source_bytes} exceed accepted limit {source_bytes_max}",
        action = "reduce source bytes below {source_bytes_max} and retry",
        fields = {
            path: optional(path),
            source_bytes: required(unsigned),
            source_bytes_max: required(unsigned),
        },
    );

    __rift_error_definition!(
        too_deep,
        slug = "rift.syntax.too_deep",
        message = "syntax tree depth exceeds accepted limit {syntax_depth_max}",
        action = "reduce syntax tree depth below {syntax_depth_max} and retry",
        fields = {
            path: required(path),
            syntax_depth_max: required(unsigned),
        },
    );

    __rift_error_definition!(
        too_many_captures,
        slug = "rift.syntax.too_many_captures",
        message = "syntax query produced more than {captures_max} captures",
        action = "reduce query captures below {captures_max} and retry",
        fields = {
            captures_max: required(unsigned),
        },
    );

    __rift_error_definition!(
        too_many_markdown_inline_ranges,
        slug = "rift.syntax.too_many_markdown_inline_ranges",
        message = "Markdown inline ranges {observed} exceed accepted limit {inline_ranges_max}",
        action = "reduce Markdown inline ranges below {inline_ranges_max} and retry",
        fields = {
            inline_ranges_max: required(unsigned),
            observed: required(unsigned),
            path: required(path),
        },
    );

    __rift_error_definition!(
        too_many_nodes,
        slug = "rift.syntax.too_many_nodes",
        message = "syntax tree node count exceeds accepted limit {syntax_nodes_max}",
        action = "reduce syntax tree node count below {syntax_nodes_max} and retry",
        fields = {
            path: required(path),
            syntax_nodes_max: required(unsigned),
        },
    );

    __rift_error_definition!(
        unknown_node_kind,
        slug = "rift.syntax.unknown_node_kind",
        message = "node kind {node_kind} is outside interpreted grammar vocabulary",
        action = "report this internal failure with its full context",
        fields = {
            node_kind: required(string),
        },
    );

    __rift_error_definition!(
        zero_limit,
        slug = "rift.syntax.zero_limit",
        message = "syntax bound {bound} is zero",
        action = "set syntax bounds above zero",
        fields = {
            bound: required(string),
        },
    );
}

/// Registered errors under `rift.tracing`.
pub mod tracing {
    use super::__rift_error_definition;

    __rift_error_definition!(
        log_batch_limit,
        slug = "rift.tracing.log_batch_limit",
        message = "log batch holds more records than its accepted limit of {maximum}",
        action = "split the batch below {maximum} records and retry",
        fields = {
            maximum: required(unsigned),
            observed: required(unsigned),
        },
    );

    __rift_error_definition!(
        log_store_failed,
        slug = "rift.tracing.log_store_failed",
        message = "metrics database operation failed: {operation} {path}: {detail}",
        action = "check the metrics database file and its permissions, then retry",
        fields = {
            detail: required(source),
            operation: required(string),
            path: required(path),
        },
    );
}
