pub use rift_error::{FieldSet, OptionalFieldSet};
use rift_error::{
    ErrorValue, IntoInteger, IntoRiftError, IntoUnsigned, Set as SetState,
    __rift_error_definition,
};
use std::{
    borrow::Borrow,
    boxed::Box,
    error::Error,
    fmt::Display,
    path::Path,
    time::Duration,
};
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
];
#[allow(missing_docs)]
pub mod analysis {
    use super::{
        Box, Display, Error, ErrorValue, IntoRiftError, IntoUnsigned, Path, SetState,
        __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error context7_entry_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.context7_entry_invalid",
        "context7.json key {key} contains an invalid entry",
        "correct the entry for key {key} and retry", 4usize]; builder Builder;
        states[State0]; complete[SetState]; fields { entry { imports { use super:: {
        Builder, Display, ErrorValue }; } output[State0]; index 0u32; key "entry";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_entry]; } file { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState]; index 1u32; key "file"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } key { imports { use super:: { Builder, Display, ErrorValue }; } output[State0];
        index 2u32; key "key"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_key]; } source { imports { use
        super:: { Box, Builder, Error, ErrorValue }; } output[State0]; index 3u32; key
        "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[maybe_source];
        } }
    }
    __rift_error_definition! {
        error context7_malformed; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.context7_malformed",
        "context7.json has an invalid JSON shape",
        "correct context7.json shape and retry", 3usize]; builder Builder;
        states[State0]; complete[SetState]; fields { file { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "file"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } key { imports { use super:: {
        Builder, Display, ErrorValue }; } output[State0]; index 1u32; key "key";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_key]; } source { imports { use super:: { Box, Builder, Error,
        ErrorValue }; } output[State0]; index 2u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error context7_oversized; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.context7_oversized",
        "context7.json exceeds its accepted byte limit",
        "reduce context7.json below its accepted byte limit and retry", 3usize]; builder
        Builder; states[State0]; complete[SetState]; fields { file { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "file"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } key { imports { use super:: {
        Builder, Display, ErrorValue }; } output[State0]; index 1u32; key "key";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_key]; } source { imports { use super:: { Box, Builder, Error,
        ErrorValue }; } output[State0]; index 2u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error context7_too_many_entries; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.context7_too_many_entries",
        "context7.json key {key} exceeds its entry limit",
        "reduce entries for key {key} below its accepted limit and retry", 3usize];
        builder Builder; states[State0]; complete[SetState]; fields { file { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "file"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } key { imports { use super:: {
        Builder, Display, ErrorValue }; } output[State0]; index 1u32; key "key";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_key]; } source { imports { use super:: { Box, Builder, Error,
        ErrorValue }; } output[State0]; index 2u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_digest_mismatch; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.documentation_digest_mismatch",
        "documentation field {field} has a digest mismatch",
        "supply bytes matching the recorded digest and retry", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_duplicate_source; imports { use super:: { Box, Display,
        Error, ErrorValue, SetState }; }
        metadata["rift.analysis.documentation_duplicate_source",
        "documentation field {field} names a duplicate source",
        "send each documentation source once and retry", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_encoding_failed; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.documentation_encoding_failed",
        "documentation field {field} could not be encoded as canonical JSON",
        "report this internal failure with its full context", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_format_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.documentation_format_invalid",
        "documentation field {field} has an invalid format",
        "correct documentation field {field} and retry", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_identity_invalid; imports { use super:: { Box, Display,
        Error, ErrorValue, SetState }; }
        metadata["rift.analysis.documentation_identity_invalid",
        "documentation field {field} has an invalid identity",
        "correct documentation field {field} and retry", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_limit_exceeded; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.documentation_limit_exceeded",
        "documentation field {field} exceeds its accepted limit",
        "reduce documentation field {field} below its accepted limit and retry", 2usize];
        builder Builder; states[State0]; complete[SetState]; fields { field { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_notebook_invalid; imports { use super:: { Box, Display,
        Error, ErrorValue, SetState }; }
        metadata["rift.analysis.documentation_notebook_invalid",
        "documentation field {field} has an invalid notebook shape",
        "correct notebook field {field} and retry", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_order_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.documentation_order_invalid",
        "documentation field {field} is not in required order",
        "order documentation field {field} and retry", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_origin_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.documentation_origin_invalid",
        "documentation field {field} conflicts with its source address",
        "correct documentation field {field} and retry", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_range_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.documentation_range_invalid",
        "documentation field {field} has a range outside source bytes",
        "correct documentation field {field} and retry", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_revision_invalid; imports { use super:: { Box, Display,
        Error, ErrorValue, SetState }; }
        metadata["rift.analysis.documentation_revision_invalid",
        "documentation field {field} has an unsupported publication revision",
        "use the supported publication revision and retry", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error documentation_target_missing; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.documentation_target_missing",
        "documentation field {field} names a missing target",
        "supply an existing target for documentation field {field}", 2usize]; builder
        Builder; states[State0]; complete[SetState]; fields { field { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error package_declarations_exceeded; imports { use super:: { Display, ErrorValue,
        Path, SetState }; } metadata["rift.analysis.package_declarations_exceeded",
        "package declaration count exceeds its accepted limit",
        "reduce package declarations below the accepted limit and retry", 2usize];
        builder Builder; states[State0]; complete[SetState]; fields { package { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "package"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } path { imports { use super:: {
        Builder, ErrorValue, Path }; } output[State0]; index 1u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error package_identity_invalid; imports { use super:: { Display, ErrorValue,
        IntoRiftError, Path, SetState }; }
        metadata["rift.analysis.package_identity_invalid",
        "package identity cannot form a source unit, resolver, or symbol",
        "correct package identity and retry", 3usize]; builder Builder; states[State0];
        complete[SetState]; fields { cause { imports { use super:: { Builder, ErrorValue,
        IntoRiftError }; } output[State0]; index 0u32; key "cause"; flags[true, false];
        bound[IntoRiftError]; value value => [ErrorValue::cause(value)];
        optional[maybe_cause]; } package { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState]; index 1u32; key "package";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[]; } path { imports { use super:: { Builder, ErrorValue, Path }; }
        output[State0]; index 2u32; key "path"; flags[true, false]; bound[AsRef < Path
        >]; value value => [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error package_input_duplicate_path; imports { use super:: { ErrorValue, Path }; }
        metadata["rift.analysis.package_input_duplicate_path",
        "package source path appears more than once",
        "send each package source path once and retry", 1usize]; builder Builder;
        states[]; complete[]; fields { path { imports { use super:: { Builder,
        ErrorValue, Path }; } output[]; index 0u32; key "path"; flags[true, false];
        bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error package_input_identity_invalid; imports { use super:: { ErrorValue,
        IntoRiftError, Path }; } metadata["rift.analysis.package_input_identity_invalid",
        "package identity cannot form a source unit",
        "correct package identity and retry", 2usize]; builder Builder; states[];
        complete[]; fields { cause { imports { use super:: { Builder, ErrorValue,
        IntoRiftError }; } output[]; index 0u32; key "cause"; flags[true, false];
        bound[IntoRiftError]; value value => [ErrorValue::cause(value)];
        optional[maybe_cause]; } path { imports { use super:: { Builder, ErrorValue, Path
        }; } output[]; index 1u32; key "path"; flags[true, false]; bound[AsRef < Path >];
        value value => [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error package_input_origin_invalid; imports { use super:: { ErrorValue, Path }; }
        metadata["rift.analysis.package_input_origin_invalid",
        "package source origin does not identify this package",
        "correct package source origin and retry", 1usize]; builder Builder; states[];
        complete[]; fields { path { imports { use super:: { Builder, ErrorValue, Path };
        } output[]; index 0u32; key "path"; flags[true, false]; bound[AsRef < Path >];
        value value => [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error package_input_too_many_bytes; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; }
        metadata["rift.analysis.package_input_too_many_bytes",
        "package source bytes exceed their accepted limit",
        "reduce package source bytes below its accepted limit and retry", 3usize];
        builder Builder; states[State0, State1, State2]; complete[SetState, SetState,
        SetState]; fields { bound { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned, SetState }; } output[SetState, State1, State2]; index 0u32; key
        "bound"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[State0, SetState, State2];
        index 1u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } observed { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, State1,
        SetState]; index 2u32; key "observed"; flags[true, false]; bound[IntoUnsigned];
        value value => [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error package_input_too_many_files; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; }
        metadata["rift.analysis.package_input_too_many_files",
        "package source count exceeds its accepted limit",
        "reduce package source count below its accepted limit and retry", 3usize];
        builder Builder; states[State0, State1, State2]; complete[SetState, SetState,
        SetState]; fields { bound { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned, SetState }; } output[SetState, State1, State2]; index 0u32; key
        "bound"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[State0, SetState, State2];
        index 1u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } observed { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, State1,
        SetState]; index 2u32; key "observed"; flags[true, false]; bound[IntoUnsigned];
        value value => [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error package_provider_failed; imports { use super:: { Box, Display, Error,
        ErrorValue, IntoRiftError, Path, SetState }; }
        metadata["rift.analysis.package_provider_failed",
        "package semantic publication failed",
        "report this internal failure with its full context", 4usize]; builder Builder;
        states[State0]; complete[SetState]; fields { cause { imports { use super:: {
        Builder, ErrorValue, IntoRiftError }; } output[State0]; index 0u32; key "cause";
        flags[true, false]; bound[IntoRiftError]; value value =>
        [ErrorValue::cause(value)]; optional[maybe_cause]; } package { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        1u32; key "package"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } path { imports { use super:: {
        Builder, ErrorValue, Path }; } output[State0]; index 2u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } source { imports { use super::
        { Box, Builder, Error, ErrorValue }; } output[State0]; index 3u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error package_syntax_unavailable; imports { use super:: { Display, ErrorValue,
        Path, SetState }; } metadata["rift.analysis.package_syntax_unavailable",
        "no shipped syntax provider accepts package file extension",
        "use a supported package file extension and retry", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { package { imports
        { use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "package"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } path { imports { use super::
        { Builder, ErrorValue, Path, SetState }; } output[State0, SetState]; index 1u32;
        key "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error source_pattern_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.analysis.source_pattern_invalid",
        "source path pattern is invalid", "correct the source path pattern and retry",
        2usize]; builder Builder; states[State0]; complete[SetState]; fields { pattern {
        imports { use super:: { Builder, Display, ErrorValue }; } output[State0]; index
        0u32; key "pattern"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_pattern]; } source { imports { use
        super:: { Box, Builder, Error, ErrorValue, SetState }; } output[SetState]; index
        1u32; key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send +
        Sync + 'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
}
#[allow(missing_docs)]
pub mod cli {
    use super::{
        Borrow, Box, Display, Duration, Error, ErrorValue, IntoRiftError, IntoUnsigned,
        Path, SetState, __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error install_home_unresolved; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.cli.install_home_unresolved",
        "the operator's home directory could not be resolved",
        "set HOME (or USERPROFILE on Windows) and retry `rift install claude --user`",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { checked {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "checked"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error install_remove_failed; imports { use super:: { Box, Error, ErrorValue,
        Path, SetState }; } metadata["rift.cli.install_remove_failed",
        "the generated Claude Code skill could not be removed: {path}",
        "ensure the target directory is writable and retry `rift install claude --remove`",
        2usize]; builder Builder; states[State0, State1]; complete[SetState, SetState];
        fields { path { imports { use super:: { Builder, ErrorValue, Path, SetState }; }
        output[SetState, State1]; index 0u32; key "path"; flags[true, false]; bound[AsRef
        < Path >]; value value => [ErrorValue::path(value)]; optional[]; } source {
        imports { use super:: { Box, Builder, Error, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error install_settings_unparsable; imports { use super:: { Box, Error,
        ErrorValue, Path, SetState }; } metadata["rift.cli.install_settings_unparsable",
        "the target settings.json could not be read as a JSON hook document: {path}",
        "fix or remove the file, then run the same `rift install` command again",
        2usize]; builder Builder; states[State0, State1]; complete[SetState, SetState];
        fields { path { imports { use super:: { Builder, ErrorValue, Path, SetState }; }
        output[SetState, State1]; index 0u32; key "path"; flags[true, false]; bound[AsRef
        < Path >]; value value => [ErrorValue::path(value)]; optional[]; } source {
        imports { use super:: { Box, Builder, Error, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error install_template_missing_tool; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.cli.install_template_missing_tool",
        "the generated Claude Code skill names a tool the served MCP surface does not have: {tool}",
        "rebuild rift so the binary and its served tool surface match, then retry `rift install claude`",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { tool {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "tool"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error install_write_failed; imports { use super:: { Box, Error, ErrorValue, Path,
        SetState }; } metadata["rift.cli.install_write_failed",
        "the generated Claude Code skill could not be written: {path}",
        "ensure the target directory is writable and retry `rift install claude`",
        2usize]; builder Builder; states[State0, State1]; complete[SetState, SetState];
        fields { path { imports { use super:: { Builder, ErrorValue, Path, SetState }; }
        output[SetState, State1]; index 0u32; key "path"; flags[true, false]; bound[AsRef
        < Path >]; value value => [ErrorValue::path(value)]; optional[]; } source {
        imports { use super:: { Box, Builder, Error, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error server_already_serving; imports { use super:: { Borrow, Display, ErrorValue
        }; } metadata["rift.cli.server_already_serving",
        "another rift server already serves this workspace",
        "connect to the listed server, or run `rift server stop` before serving again",
        3usize]; builder Builder; states[]; complete[]; fields { detail { imports { use
        super:: { Builder, Display, ErrorValue }; } output[]; index 0u32; key "detail";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_detail]; } listening { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "listening"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_listening]; } pid { imports { use super:: { Borrow, Builder,
        ErrorValue }; } output[]; index 2u32; key "pid"; flags[true, false]; bound[Borrow
        < u32 >]; value value => [ErrorValue::pid(value)]; optional[maybe_pid]; } }
    }
    __rift_error_definition! {
        error server_election_unreleased; imports { use super:: { Borrow, Duration,
        ErrorValue, SetState }; } metadata["rift.cli.server_election_unreleased",
        "server stopped answering its port but still holds the election: process {pid}, waited {waited}",
        "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
        2usize]; builder Builder; states[State0, State1]; complete[SetState, SetState];
        fields { pid { imports { use super:: { Borrow, Builder, ErrorValue, SetState }; }
        output[SetState, State1]; index 0u32; key "pid"; flags[true, false]; bound[Borrow
        < u32 >]; value value => [ErrorValue::pid(value)]; optional[]; } waited { imports
        { use super:: { Borrow, Builder, Duration, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "waited"; flags[true, false];
        bound[Borrow < Duration >]; value value => [ErrorValue::duration(value)];
        optional[]; } }
    }
    __rift_error_definition! {
        error server_logs_unavailable; imports { use super:: { Display, ErrorValue,
        IntoRiftError }; } metadata["rift.cli.server_logs_unavailable",
        "the workspace's recorded server logs could not be read",
        "ensure no other process holds `.rift/db` exclusively and retry", 3usize];
        builder Builder; states[]; complete[]; fields { detail { imports { use super:: {
        Builder, Display, ErrorValue }; } output[]; index 0u32; key "detail"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_detail]; } operation { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "operation"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_operation]; } source { imports { use super:: { Builder,
        ErrorValue, IntoRiftError }; } output[]; index 2u32; key "source"; flags[true,
        false]; bound[IntoRiftError]; value value => [ErrorValue::cause(value)];
        optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error server_spawn_failed; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.cli.server_spawn_failed",
        "the rift server process could not be started: {source}",
        "check that the rift binary is runnable, or run `rift server start --foreground` to serve in this process",
        2usize]; builder Builder; states[State0, State1]; complete[SetState, SetState];
        fields { operation { imports { use super:: { Builder, Display, ErrorValue,
        SetState }; } output[SetState, State1]; index 0u32; key "operation"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } source { imports { use super:: { Box, Builder, Error, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error server_start_exited; imports { use super:: { Borrow, ErrorValue, SetState
        }; } metadata["rift.cli.server_start_exited",
        "the spawned rift server exited before publishing its lock document: process {pid}",
        "read `.rift/server.stderr`, or run `rift server logs --level error`, for what the server reported before it exited",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { pid {
        imports { use super:: { Borrow, Builder, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "pid"; flags[true, false]; bound[Borrow < u32
        >]; value value => [ErrorValue::pid(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error server_start_timed_out; imports { use super:: { Borrow, Duration,
        ErrorValue, SetState }; } metadata["rift.cli.server_start_timed_out",
        "the started rift server did not report serving within {waited}",
        "run `rift server start --foreground` to see the server's diagnostics on stderr",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { waited {
        imports { use super:: { Borrow, Builder, Duration, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "waited"; flags[true, false]; bound[Borrow <
        Duration >]; value value => [ErrorValue::duration(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error server_stop_refused; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.cli.server_stop_refused",
        "the server refused the stop request with status {status}",
        "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
        2usize]; builder Builder; states[State0]; complete[SetState]; fields { detail {
        imports { use super:: { Builder, Display, ErrorValue }; } output[State0]; index
        0u32; key "detail"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_detail]; } status { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState];
        index 1u32; key "status"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error server_stop_request_failed; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.cli.server_stop_request_failed",
        "the server stop request could not be delivered: {source}",
        "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
        2usize]; builder Builder; states[State0, State1]; complete[SetState, SetState];
        fields { operation { imports { use super:: { Builder, Display, ErrorValue,
        SetState }; } output[SetState, State1]; index 0u32; key "operation"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } source { imports { use super:: { Box, Builder, Error, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error server_stop_timed_out; imports { use super:: { Borrow, Display, Duration,
        ErrorValue, SetState }; } metadata["rift.cli.server_stop_timed_out",
        "the server accepted the stop request but kept serving after {waited}",
        "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
        4usize]; builder Builder; states[State0]; complete[SetState]; fields { detail {
        imports { use super:: { Builder, Display, ErrorValue }; } output[State0]; index
        0u32; key "detail"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_detail]; } listening { imports { use
        super:: { Builder, Display, ErrorValue }; } output[State0]; index 1u32; key
        "listening"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_listening]; } pid { imports { use
        super:: { Borrow, Builder, ErrorValue }; } output[State0]; index 2u32; key "pid";
        flags[true, false]; bound[Borrow < u32 >]; value value =>
        [ErrorValue::pid(value)]; optional[maybe_pid]; } waited { imports { use super:: {
        Borrow, Builder, Duration, ErrorValue, SetState }; } output[SetState]; index
        3u32; key "waited"; flags[true, false]; bound[Borrow < Duration >]; value value
        => [ErrorValue::duration(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_archive_contents_invalid; imports {}
        metadata["rift.cli.update_archive_contents_invalid",
        "release archive contents are invalid: expected exactly one binary, README.md, and LICENSE.md member; retry `rift update`",
        "retry `rift update`", 0usize]; builder Builder; states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error update_archive_extraction_failed; imports { use super:: { Box, Error,
        ErrorValue, SetState }; } metadata["rift.cli.update_archive_extraction_failed",
        "release archive could not be extracted: retry `rift update`; if this persists the download may be corrupted",
        "retry `rift update`; if this persists the download may be corrupted", 1usize];
        builder Builder; states[State0]; complete[SetState]; fields { source { imports {
        use super:: { Box, Builder, Error, ErrorValue, SetState }; } output[SetState];
        index 0u32; key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send
        + Sync + 'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_archive_file_inspection_failed; imports { use super:: { Box, Error,
        ErrorValue, Path, SetState }; }
        metadata["rift.cli.update_archive_file_inspection_failed",
        "downloaded release file at `{path}` could not be inspected: {source}: retry `rift update`",
        "retry `rift update`", 2usize]; builder Builder; states[State0, State1];
        complete[SetState, SetState]; fields { path { imports { use super:: { Builder,
        ErrorValue, Path, SetState }; } output[SetState, State1]; index 0u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue, SetState }; } output[State0, SetState]; index 1u32;
        key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_archive_file_not_regular; imports { use super:: { ErrorValue, Path,
        SetState }; } metadata["rift.cli.update_archive_file_not_regular",
        "downloaded release file at `{path}` is not a regular file: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
        "retry `rift update` or create an issue", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { path { imports { use super:: {
        Builder, ErrorValue, Path, SetState }; } output[SetState]; index 0u32; key
        "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_archive_file_size_invalid; imports { use super:: { ErrorValue,
        IntoUnsigned, Path, SetState }; }
        metadata["rift.cli.update_archive_file_size_invalid",
        "downloaded release file at `{path}` has incorrect size of {size} bytes, expected between 1 and {bytes_max} bytes: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
        "retry `rift update` or create an issue", 3usize]; builder Builder;
        states[State0, State1, State2]; complete[SetState, SetState, SetState]; fields {
        bytes_max { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState
        }; } output[SetState, State1, State2]; index 0u32; key "bytes_max"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } path { imports { use super:: { Builder, ErrorValue, Path, SetState
        }; } output[State0, SetState, State2]; index 1u32; key "path"; flags[true,
        false]; bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[]; } size { imports { use super:: { Builder, ErrorValue, IntoUnsigned,
        SetState }; } output[State0, State1, SetState]; index 2u32; key "size";
        flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_archive_member_too_large; imports { use super:: { ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.cli.update_archive_member_too_large",
        "release archive member is empty or exceeds {bytes_max} bytes: retry `rift update`; if this persists the release may be malformed",
        "retry `rift update`; if this persists the release may be malformed", 1usize];
        builder Builder; states[State0]; complete[SetState]; fields { bytes_max { imports
        { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[SetState]; index 0u32; key "bytes_max"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        }
    }
    __rift_error_definition! {
        error update_binary_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.cli.update_binary_invalid",
        "current Rift executable (invoked as `{invoked_as}`) could not be located: {source}: reinstall Rift if the binary was moved or deleted",
        "reinstall Rift if the binary was moved or deleted", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { invoked_as {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1]; index 0u32; key "invoked_as"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } source
        { imports { use super:: { Box, Builder, Error, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_checksum_manifest_invalid; imports {}
        metadata["rift.cli.update_checksum_manifest_invalid",
        "release checksum manifest is invalid: expected `sha256sum`-format lines naming the release archive exactly once; retry `rift update`",
        "retry `rift update`", 0usize]; builder Builder; states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error update_checksum_mismatch; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.cli.update_checksum_mismatch",
        "the downloaded release does not match its published checksum: expected {expected}, actual {actual}; retry `rift update`, and raise an issue at https://github.com/volarized/rift/issues if the mismatch repeats",
        "retry `rift update` and raise an issue if mismatch repeats", 2usize]; builder
        Builder; states[State0, State1]; complete[SetState, SetState]; fields { actual {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1]; index 0u32; key "actual"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; }
        expected { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "expected"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_checksum_read_failed; imports { use super:: { Box, Error,
        ErrorValue, SetState }; } metadata["rift.cli.update_checksum_read_failed",
        "release checksum could not be verified: retry `rift update`",
        "retry `rift update`", 1usize]; builder Builder; states[State0];
        complete[SetState]; fields { source { imports { use super:: { Box, Builder,
        Error, ErrorValue, SetState }; } output[SetState]; index 0u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_download_failed; imports { use super:: { Box, Error, ErrorValue,
        SetState }; } metadata["rift.cli.update_download_failed",
        "release download failed: check network access to github.com and retry `rift update`",
        "check network access to github.com and retry `rift update`", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { source { imports { use
        super:: { Box, Builder, Error, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send +
        Sync + 'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_download_too_large; imports { use super:: { ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.cli.update_download_too_large",
        "release download was empty or exceeded {bytes_max} bytes: retry `rift update`; if this persists the release assets may be malformed",
        "retry `rift update`; if this persists the release assets may be malformed",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { bytes_max
        { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[SetState]; index 0u32; key "bytes_max"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        }
    }
    __rift_error_definition! {
        error update_prerelease_unsupported; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.cli.update_prerelease_unsupported",
        "release tag `{tag}` is a pre-release or build: only stable releases of the form `vMAJOR.MINOR.PATCH` are supported",
        "use a stable release tag of the form `vMAJOR.MINOR.PATCH`", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { tag { imports { use super::
        { Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "tag"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_publish_copy_failed; imports { use super:: { ErrorValue,
        IntoRiftError, Path, SetState }; }
        metadata["rift.cli.update_publish_copy_failed",
        "Rift update could not be published: copying the downloaded binary into `{path}` failed: ensure the directory is writable and has free space, then retry `rift update`",
        "ensure the directory is writable and has free space, then retry `rift update`",
        2usize]; builder Builder; states[State0, State1]; complete[SetState, SetState];
        fields { cause { imports { use super:: { Builder, ErrorValue, IntoRiftError,
        SetState }; } output[SetState, State1]; index 0u32; key "cause"; flags[true,
        false]; bound[IntoRiftError]; value value => [ErrorValue::cause(value)];
        optional[]; } path { imports { use super:: { Builder, ErrorValue, Path, SetState
        }; } output[State0, SetState]; index 1u32; key "path"; flags[true, false];
        bound[AsRef < Path >]; value value => [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_publish_failed; imports { use super:: { Box, Display, Error,
        ErrorValue, Path, SetState }; } metadata["rift.cli.update_publish_failed",
        "Rift update could not be published: {operation} `{path}` failed: {source}: ensure the directory is writable and retry `rift update`",
        "ensure the directory is writable and retry `rift update`", 3usize]; builder
        Builder; states[State0, State1, State2]; complete[SetState, SetState, SetState];
        fields { operation { imports { use super:: { Builder, Display, ErrorValue,
        SetState }; } output[SetState, State1, State2]; index 0u32; key "operation";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[]; } path { imports { use super:: { Builder, ErrorValue, Path, SetState
        }; } output[State0, SetState, State2]; index 1u32; key "path"; flags[true,
        false]; bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[]; } source { imports { use super:: { Box, Builder, Error, ErrorValue,
        SetState }; } output[State0, State1, SetState]; index 2u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_publish_parent_missing; imports { use super:: { ErrorValue, Path,
        SetState }; } metadata["rift.cli.update_publish_parent_missing",
        "current executable `{path}` has no parent directory: install Rift in a regular directory before updating",
        "install Rift in a regular directory before updating", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { path { imports { use super:: {
        Builder, ErrorValue, Path, SetState }; } output[SetState]; index 0u32; key
        "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_publish_pending_cleanup; imports { use super:: { ErrorValue, Path,
        SetState }; } metadata["rift.cli.update_publish_pending_cleanup",
        "another Rift update is pending cleanup: retry after the previous Rift process exits, or delete `{path}`",
        "retry after the previous Rift process exits, or delete the pending file",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path {
        imports { use super:: { Builder, ErrorValue, Path, SetState }; }
        output[SetState]; index 0u32; key "path"; flags[true, false]; bound[AsRef < Path
        >]; value value => [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_release_file_inspection_failed; imports { use super:: { Box, Error,
        ErrorValue, Path, SetState }; }
        metadata["rift.cli.update_release_file_inspection_failed",
        "downloaded release file at `{path}` could not be inspected: {source}: retry `rift update`",
        "retry `rift update`", 2usize]; builder Builder; states[State0, State1];
        complete[SetState, SetState]; fields { path { imports { use super:: { Builder,
        ErrorValue, Path, SetState }; } output[SetState, State1]; index 0u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue, SetState }; } output[State0, SetState]; index 1u32;
        key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_release_file_not_regular; imports { use super:: { ErrorValue, Path,
        SetState }; } metadata["rift.cli.update_release_file_not_regular",
        "downloaded release file at `{path}` is not a regular file: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
        "retry `rift update` or create an issue", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { path { imports { use super:: {
        Builder, ErrorValue, Path, SetState }; } output[SetState]; index 0u32; key
        "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_release_file_size_invalid; imports { use super:: { ErrorValue,
        IntoUnsigned, Path, SetState }; }
        metadata["rift.cli.update_release_file_size_invalid",
        "downloaded release file at `{path}` has incorrect size of {size} bytes, expected between 1 and {bytes_max} bytes: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
        "retry `rift update` or create an issue", 3usize]; builder Builder;
        states[State0, State1, State2]; complete[SetState, SetState, SetState]; fields {
        bytes_max { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState
        }; } output[SetState, State1, State2]; index 0u32; key "bytes_max"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } path { imports { use super:: { Builder, ErrorValue, Path, SetState
        }; } output[State0, SetState, State2]; index 1u32; key "path"; flags[true,
        false]; bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[]; } size { imports { use super:: { Builder, ErrorValue, IntoUnsigned,
        SetState }; } output[State0, State1, SetState]; index 2u32; key "size";
        flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_release_metadata_invalid; imports { use super:: { Box, Error,
        ErrorValue, SetState }; } metadata["rift.cli.update_release_metadata_invalid",
        "latest release metadata is invalid: retry `rift update` or check https://github.com/volarized/rift/releases",
        "retry `rift update` or check the release page", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { source { imports { use super:: {
        Box, Builder, Error, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_release_tag_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.cli.update_release_tag_invalid",
        "release tag `{tag}` is invalid: expected the form `vMAJOR.MINOR.PATCH`, such as `v0.0.2`",
        "use a stable release tag of the form `vMAJOR.MINOR.PATCH`", 2usize]; builder
        Builder; states[State0]; complete[SetState]; fields { source { imports { use
        super:: { Box, Builder, Error, ErrorValue }; } output[State0]; index 0u32; key
        "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[maybe_source];
        } tag { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 1u32; key "tag"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_rollback_cleanup_failed; imports { use super:: { Box, Error,
        ErrorValue, Path, SetState }; }
        metadata["rift.cli.update_rollback_cleanup_failed",
        "We were not able to clean up the old binary at `{path}`: {source}: delete the file manually",
        "delete the file manually", 2usize]; builder Builder; states[State0, State1];
        complete[SetState, SetState]; fields { path { imports { use super:: { Builder,
        ErrorValue, Path, SetState }; } output[SetState, State1]; index 0u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue, SetState }; } output[State0, SetState]; index 1u32;
        key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_rollback_failed; imports { use super:: { Box, Error, ErrorValue,
        Path, SetState }; } metadata["rift.cli.update_rollback_failed",
        "Rift update publish and rollback of `{path}` both failed: reinstall Rift from an official release",
        "reinstall Rift from an official release", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { path { imports {
        use super:: { Builder, ErrorValue, Path, SetState }; } output[SetState, State1];
        index 0u32; key "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue, SetState }; } output[State0, SetState]; index 1u32;
        key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_staging_failed; imports { use super:: { Box, Display, Error,
        ErrorValue, Path, SetState }; } metadata["rift.cli.update_staging_failed",
        "update staging directory could not be created under `{path}` ({space}): {source}: ensure the directory is writable and has free space, then retry `rift update`",
        "ensure the directory is writable and has free space, then retry `rift update`",
        3usize]; builder Builder; states[State0, State1, State2]; complete[SetState,
        SetState, SetState]; fields { path { imports { use super:: { Builder, ErrorValue,
        Path, SetState }; } output[SetState, State1, State2]; index 0u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue, SetState }; } output[State0, SetState, State2]; index
        1u32; key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send +
        Sync + 'static >>]; value value => [ErrorValue::source(value)]; optional[]; }
        space { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, State1, SetState]; index 2u32; key "space"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error update_version_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, Path, SetState }; } metadata["rift.cli.update_version_invalid",
        "installed Rift version `{raw}` at `{path}` is invalid: {source}: reinstall Rift from an official release",
        "reinstall Rift from an official release", 3usize]; builder Builder;
        states[State0, State1, State2]; complete[SetState, SetState, SetState]; fields {
        path { imports { use super:: { Builder, ErrorValue, Path, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "path"; flags[true, false];
        bound[AsRef < Path >]; value value => [ErrorValue::path(value)]; optional[]; }
        raw { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, SetState, State2]; index 1u32; key "raw"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } source
        { imports { use super:: { Box, Builder, Error, ErrorValue, SetState }; }
        output[State0, State1, SetState]; index 2u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } }
    }
}
#[allow(missing_docs)]
pub mod core {
    use super::{
        Box, Display, Error, ErrorValue, IntoRiftError, IntoUnsigned, SetState,
        __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error configuration_command_argument_oversized; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_command_argument_oversized",
        "configured command argument exceeds its byte limit",
        "correct the reported configuration field, then retry", 3usize]; builder Builder;
        states[]; complete[]; fields { bytes { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "bytes"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_bytes]; } bytes_max { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "bytes_max"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_bytes_max]; } field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 2u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } }
    }
    __rift_error_definition! {
        error configuration_command_program_absolute; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_command_program_absolute",
        "configured command executable is an absolute path",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } program { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "program"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_program]; } }
    }
    __rift_error_definition! {
        error configuration_command_program_dot_segment; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_command_program_dot_segment",
        "configured command executable contains a dot segment",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } program { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "program"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_program]; } }
    }
    __rift_error_definition! {
        error configuration_command_program_empty; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_command_program_empty",
        "configured command has no executable",
        "correct the reported configuration field, then retry", 1usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } }
    }
    __rift_error_definition! {
        error configuration_command_program_oversized; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_command_program_oversized",
        "configured command executable exceeds its byte limit",
        "correct the reported configuration field, then retry", 3usize]; builder Builder;
        states[]; complete[]; fields { bytes { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "bytes"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_bytes]; } bytes_max { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "bytes_max"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_bytes_max]; } field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 2u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } }
    }
    __rift_error_definition! {
        error configuration_command_program_whitespace; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_command_program_whitespace",
        "configured command executable contains whitespace",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } program { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "program"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_program]; } }
    }
    __rift_error_definition! {
        error configuration_embedding_endpoint_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_embedding_endpoint_invalid",
        "embedding endpoint is not an accepted URL",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } value { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "value"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_value]; } }
    }
    __rift_error_definition! {
        error configuration_embedding_identifier_invalid; imports { use super:: {
        Display, ErrorValue }; }
        metadata["rift.core.configuration_embedding_identifier_invalid",
        "embedding identifier value is empty or too long",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } value { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "value"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_value]; } }
    }
    __rift_error_definition! {
        error configuration_embedding_model_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_embedding_model_invalid",
        "embedding model value does not match its configured kind",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } value { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "value"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_value]; } }
    }
    __rift_error_definition! {
        error configuration_file_name_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_file_name_invalid",
        "excluded lockfile value is not one file name",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } name { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "name"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_name]; } }
    }
    __rift_error_definition! {
        error configuration_history_cpu_share_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_history_cpu_share_invalid",
        "history CPU share is outside its accepted range",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } share { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "share"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_share]; } }
    }
    __rift_error_definition! {
        error configuration_history_release_pattern_invalid; imports { use super:: {
        Display, ErrorValue }; }
        metadata["rift.core.configuration_history_release_pattern_invalid",
        "history release pattern is invalid",
        "correct the reported configuration field, then retry", 3usize]; builder Builder;
        states[]; complete[]; fields { detail { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "detail"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_detail]; } field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } pattern { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 2u32; key "pattern"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_pattern]; } }
    }
    __rift_error_definition! {
        error configuration_history_releases_missing; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_history_releases_missing",
        "selective history strategy has no release pattern",
        "correct the reported configuration field, then retry", 1usize]; builder Builder;
        states[]; complete[]; fields { fields { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "fields"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_fields]; } }
    }
    __rift_error_definition! {
        error configuration_history_releases_outside_selective; imports { use super:: {
        Display, ErrorValue }; }
        metadata["rift.core.configuration_history_releases_outside_selective",
        "history release patterns require selective strategy",
        "correct the reported configuration field, then retry", 1usize]; builder Builder;
        states[]; complete[]; fields { fields { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "fields"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_fields]; } }
    }
    __rift_error_definition! {
        error configuration_invalid; imports { use super:: { Display, ErrorValue }; }
        metadata["rift.core.configuration_invalid",
        "workspace configuration failed validation",
        "correct the reported configuration field, then retry", 13usize]; builder
        Builder; states[]; complete[]; fields { detail { imports { use super:: { Builder,
        Display, ErrorValue }; } output[]; index 0u32; key "detail"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_detail]; } field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } file { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 2u32; key "file"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_file]; } first { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 3u32; key "first"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_first]; } key { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 4u32; key "key"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[maybe_key];
        } language { imports { use super:: { Builder, Display, ErrorValue }; } output[];
        index 5u32; key "language"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_language]; } lsp { imports { use
        super:: { Builder, Display, ErrorValue }; } output[]; index 6u32; key "lsp";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_lsp]; } name { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 7u32; key "name"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_name]; } pattern { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 8u32; key "pattern"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_pattern]; } range { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 9u32; key "range"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_range]; } second { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 10u32; key "second"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_second]; } value { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 11u32; key "value"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_value]; } variables { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 12u32; key "variables"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_variables]; } }
    }
    __rift_error_definition! {
        error configuration_is_directory; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.configuration_is_directory",
        "workspace configuration path is a directory",
        "correct the reported configuration field, then retry", 3usize]; builder Builder;
        states[State0, State1, State2]; complete[SetState, SetState, SetState]; fields {
        detail { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "detail"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } file {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, SetState, State2]; index 1u32; key "file"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } path {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, State1, SetState]; index 2u32; key "path"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error configuration_language_identity_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_language_identity_invalid",
        "language table key is not a canonical language identity",
        "correct the reported configuration field, then retry", 1usize]; builder Builder;
        states[]; complete[]; fields { language { imports { use super:: { Builder,
        Display, ErrorValue }; } output[]; index 0u32; key "language"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_language]; } }
    }
    __rift_error_definition! {
        error configuration_language_include_duplicate; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_language_include_duplicate",
        "two language entries use same include pattern",
        "correct the reported configuration field, then retry", 3usize]; builder Builder;
        states[]; complete[]; fields { first { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "first"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_first]; } pattern { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "pattern"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_pattern]; } second { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 2u32; key "second"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_second]; } }
    }
    __rift_error_definition! {
        error configuration_language_lsp_unknown; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_language_lsp_unknown",
        "language names an LSP process that is not declared",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { language { imports { use super:: { Builder,
        Display, ErrorValue }; } output[]; index 0u32; key "language"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_language]; } lsp { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "lsp"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[maybe_lsp];
        } }
    }
    __rift_error_definition! {
        error configuration_limit_out_of_range; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_limit_out_of_range",
        "numeric configuration value is outside its documented range",
        "correct the reported configuration field, then retry", 3usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } range { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "range"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_range]; } value { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 2u32; key "value"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_value]; } }
    }
    __rift_error_definition! {
        error configuration_log_capture_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_log_capture_invalid",
        "logs.capture is not a tracing filter directive",
        "correct the reported configuration field, then retry", 3usize]; builder Builder;
        states[]; complete[]; fields { capture { imports { use super:: { Builder,
        Display, ErrorValue }; } output[]; index 0u32; key "capture"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_capture]; } detail { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "detail"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_detail]; } field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 2u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } }
    }
    __rift_error_definition! {
        error configuration_lsp_embedded_extras; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_lsp_embedded_extras",
        "embedded LSP engine has a spawned-process setting",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } lsp { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "lsp"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[maybe_lsp];
        } }
    }
    __rift_error_definition! {
        error configuration_lsp_engine_missing; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_lsp_engine_missing",
        "LSP table selects no engine",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { fields { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "fields"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_fields]; } lsp { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "lsp"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[maybe_lsp];
        } }
    }
    __rift_error_definition! {
        error configuration_lsp_engine_selection_conflict; imports { use super:: {
        Display, ErrorValue }; }
        metadata["rift.core.configuration_lsp_engine_selection_conflict",
        "LSP table selects command and embedded engines",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { fields { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "fields"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_fields]; } lsp { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "lsp"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[maybe_lsp];
        } }
    }
    __rift_error_definition! {
        error configuration_lsp_environment_key_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_lsp_environment_key_invalid",
        "LSP environment key is empty or contains a forbidden character",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { key { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "key"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[maybe_key];
        } lsp { imports { use super:: { Builder, Display, ErrorValue }; } output[]; index
        1u32; key "lsp"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_lsp]; } }
    }
    __rift_error_definition! {
        error configuration_lsp_initialization_options_not_object; imports { use super::
        { Display, ErrorValue }; }
        metadata["rift.core.configuration_lsp_initialization_options_not_object",
        "LSP initialization options are not a JSON object",
        "correct the reported configuration field, then retry", 1usize]; builder Builder;
        states[]; complete[]; fields { lsp { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "lsp"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[maybe_lsp];
        } }
    }
    __rift_error_definition! {
        error configuration_lsp_name_invalid; imports { use super:: { Display, ErrorValue
        }; } metadata["rift.core.configuration_lsp_name_invalid",
        "LSP process name is not a lowercase word",
        "correct the reported configuration field, then retry", 1usize]; builder Builder;
        states[]; complete[]; fields { name { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "name"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_name]; } }
    }
    __rift_error_definition! {
        error configuration_malformed; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.configuration_malformed",
        "workspace configuration does not match its documented shape",
        "correct the reported configuration field, then retry", 5usize]; builder Builder;
        states[State0]; complete[SetState]; fields { accepted { imports { use super:: {
        Builder, Display, ErrorValue }; } output[State0]; index 0u32; key "accepted";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_accepted]; } example { imports { use super:: { Builder, Display,
        ErrorValue }; } output[State0]; index 1u32; key "example"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_example]; } file { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState]; index 2u32; key "file"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } key { imports { use super:: { Builder, Display, ErrorValue }; } output[State0];
        index 3u32; key "key"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_key]; } location { imports { use
        super:: { Builder, Display, ErrorValue }; } output[State0]; index 4u32; key
        "location"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_location]; } }
    }
    __rift_error_definition! {
        error configuration_oversized; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.core.configuration_oversized",
        "workspace configuration exceeds its accepted byte limit",
        "correct the reported configuration field, then retry", 3usize]; builder Builder;
        states[State0, State1, State2]; complete[SetState, SetState, SetState]; fields {
        bytes { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "bytes"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        bytes_max { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState
        }; } output[State0, SetState, State2]; index 1u32; key "bytes_max"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } file { imports { use super:: { Builder, Display, ErrorValue,
        SetState }; } output[State0, State1, SetState]; index 2u32; key "file";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[]; } }
    }
    __rift_error_definition! {
        error configuration_package_selector_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_package_selector_invalid",
        "dependency package has conflicting or missing version selector",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } package { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "package"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_package]; } }
    }
    __rift_error_definition! {
        error configuration_path_pattern_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_path_pattern_invalid",
        "configuration path pattern breaks forward-slash path rules",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } pattern { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "pattern"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_pattern]; } }
    }
    __rift_error_definition! {
        error configuration_port_range_inverted; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_port_range_inverted",
        "server port range maximum is below minimum",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[]; complete[]; fields { max { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "max"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[maybe_max];
        } min { imports { use super:: { Builder, Display, ErrorValue }; } output[]; index
        1u32; key "min"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_min]; } }
    }
    __rift_error_definition! {
        error configuration_port_selection_conflict; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_port_selection_conflict",
        "server selects port and port range together",
        "correct the reported configuration field, then retry", 1usize]; builder Builder;
        states[]; complete[]; fields { fields { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "fields"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_fields]; } }
    }
    __rift_error_definition! {
        error configuration_search_weights_invalid; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.core.configuration_search_weights_invalid",
        "search ranking weights are not a usable set",
        "correct the reported configuration field, then retry", 3usize]; builder Builder;
        states[]; complete[]; fields { identifier_weight { imports { use super:: {
        Builder, Display, ErrorValue }; } output[]; index 0u32; key "identifier_weight";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_identifier_weight]; } lexical_weight { imports { use super:: {
        Builder, Display, ErrorValue }; } output[]; index 1u32; key "lexical_weight";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_lexical_weight]; } vector_weight { imports { use super:: {
        Builder, Display, ErrorValue }; } output[]; index 2u32; key "vector_weight";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_vector_weight]; } }
    }
    __rift_error_definition! {
        error configuration_unit_parse; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.configuration_unit_parse",
        "configuration value does not use its required unit form",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { expected { imports
        { use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "expected"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } value { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "value"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error configuration_unreadable; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.core.configuration_unreadable",
        "workspace configuration could not be read",
        "check filesystem permissions and free space, then retry", 4usize]; builder
        Builder; states[State0, State1, State2]; complete[SetState, SetState, SetState];
        fields { file { imports { use super:: { Builder, Display, ErrorValue, SetState };
        } output[SetState, State1, State2]; index 0u32; key "file"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } io {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, SetState, State2]; index 1u32; key "io"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } path {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, State1, SetState]; index 2u32; key "path"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } source
        { imports { use super:: { Box, Builder, Error, ErrorValue }; } output[State0,
        State1, State2]; index 3u32; key "source"; flags[true, false]; bound[Into < Box <
        dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error configuration_variable_malformed; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.core.configuration_variable_malformed",
        "configuration variable value does not match its documented shape",
        "correct the reported configuration field, then retry", 4usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { accepted { imports
        { use super:: { Builder, Display, ErrorValue }; } output[State0, State1]; index
        0u32; key "accepted"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_accepted]; } example { imports { use
        super:: { Builder, Display, ErrorValue }; } output[State0, State1]; index 1u32;
        key "example"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[maybe_example]; } key { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState, State1];
        index 2u32; key "key"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } variable { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[State0, SetState]; index 3u32;
        key "variable"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error configuration_variable_not_unicode; imports { use super:: { Display,
        ErrorValue, SetState }; }
        metadata["rift.core.configuration_variable_not_unicode",
        "configuration variable value is not UTF-8",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { detail { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "detail"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } variable { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "variable"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error configuration_variable_unknown; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.core.configuration_variable_unknown",
        "configuration variable names no declared key",
        "correct the reported configuration field, then retry", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { accepted { imports
        { use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "accepted"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } variable { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "variable"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_duplicate_fact; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.contribution_duplicate_fact",
        "contribution field {field} violates rule: portable facets contain duplicates",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid_kind; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.contribution_invalid_kind",
        "contribution field {field} violates rule: exact kind does not use provider-kind syntax",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid_language; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.contribution_invalid_language",
        "contribution field {field} violates rule: language identity does not use canonical lowercase syntax",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid_name; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.contribution_invalid_name",
        "contribution field {field} violates rule: portable name is empty, oversized, or contains control text",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid_namespace; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.core.contribution_invalid_namespace",
        "contribution field {field} violates rule: provider-specific fact key is not reverse-domain syntax",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid_namespace_version; imports { use super:: { Display,
        ErrorValue, SetState }; }
        metadata["rift.core.contribution_invalid_namespace_version",
        "contribution field {field} violates rule: provider-specific fact version is zero",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid_origin; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.contribution_invalid_origin",
        "contribution field {field} violates rule: source location and source kind disagree",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid_record; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.contribution_invalid_record",
        "contribution field {field} violates rule: normalized record state, identity, or members disagree",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid_reference; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.core.contribution_invalid_reference",
        "contribution field {field} violates rule: reference targets are empty, duplicated, or oversized",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid_source_range; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.core.contribution_invalid_source_range",
        "contribution field {field} violates rule: source range is empty or reversed",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_too_many_facts; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.contribution_too_many_facts",
        "contribution field {field} violates rule: portable facts exceed their count bound",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_too_many_namespaced_facts; imports { use super:: { Display,
        ErrorValue, SetState }; }
        metadata["rift.core.contribution_too_many_namespaced_facts",
        "contribution field {field} violates rule: provider-specific facts exceed count or byte bound",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_too_much_evidence; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.core.contribution_too_much_evidence",
        "contribution field {field} violates rule: equivalence evidence exceeds its count bound",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_unbound_identity; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.contribution_unbound_identity",
        "contribution field {field} violates rule: identity anchor has no exact source binding",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error identity_invalid; imports {} metadata["rift.core.identity_invalid",
        "identity is empty or contains a control character",
        "correct the reported field and resend the request", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error path_absolute; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.core.path_absolute", "{path_kind} path is absolute",
        "use a workspace-relative path with `/` separators and no `.` or `..` components",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path_kind
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path_kind"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error path_backslash; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.core.path_backslash", "{path_kind} path contains a backslash",
        "use a workspace-relative path with `/` separators and no `.` or `..` components",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path_kind
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path_kind"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error path_control_character; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.path_control_character",
        "{path_kind} path contains a control character",
        "use a workspace-relative path with `/` separators and no `.` or `..` components",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path_kind
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path_kind"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error path_dot_segment; imports { use super:: { Display, ErrorValue, SetState };
        } metadata["rift.core.path_dot_segment",
        "{path_kind} path contains a dot segment",
        "use a workspace-relative path with `/` separators and no `.` or `..` components",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path_kind
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path_kind"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error path_empty; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.core.path_empty", "{path_kind} path is empty",
        "use a workspace-relative path with `/` separators and no `.` or `..` components",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path_kind
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path_kind"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error path_empty_segment; imports { use super:: { Display, ErrorValue, SetState
        }; } metadata["rift.core.path_empty_segment",
        "project path contains an empty segment",
        "use a workspace-relative path with `/` separators and no `.` or `..` components",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path_kind
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path_kind"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error path_non_canonical_unicode; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.path_non_canonical_unicode",
        "project path does not use Unicode NFC",
        "use a workspace-relative path with `/` separators and no `.` or `..` components",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path_kind
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path_kind"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error path_rift_state; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.core.path_rift_state", "project path addresses Rift state",
        "use a workspace-relative path with `/` separators and no `.` or `..` components",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path_kind
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path_kind"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error path_too_long; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.core.path_too_long",
        "{path_kind} path exceeds its accepted byte limit",
        "use a workspace-relative path with `/` separators and no `.` or `..` components",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path_kind
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path_kind"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error resolver_id_empty; imports { use super:: { Display, ErrorValue, SetState };
        } metadata["rift.core.resolver_id_empty", "source resolver identity is empty",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { identity { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "identity"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error resolver_id_invalid_character; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.resolver_id_invalid_character",
        "source resolver identity is not canonical lowercase syntax",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { identity { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "identity"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error resolver_id_too_long; imports { use super:: { Display, ErrorValue, SetState
        }; } metadata["rift.core.resolver_id_too_long",
        "source resolver identity exceeds its accepted byte limit",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { identity { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "identity"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error revision_zero; imports {} metadata["rift.core.revision_zero",
        "revision is zero", "correct the reported field and resend the request", 0usize];
        builder Builder; states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error source_unit_id_invalid_address; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.core.source_unit_id_invalid_address",
        "source-unit identity does not use canonical Rift address structure",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { identity { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "identity"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error source_unit_id_invalid_encoding; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.core.source_unit_id_invalid_encoding",
        "source-unit key has malformed percent encoding or invalid UTF-8",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { identity { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "identity"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error source_unit_id_invalid_key; imports { use super:: { Display, ErrorValue,
        IntoRiftError, SetState }; } metadata["rift.core.source_unit_id_invalid_key",
        "source-unit key breaks source-path rules: {cause}",
        "correct the reported field and resend the request", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { cause { imports {
        use super:: { Builder, ErrorValue, IntoRiftError, SetState }; } output[SetState,
        State1]; index 0u32; key "cause"; flags[true, false]; bound[IntoRiftError]; value
        value => [ErrorValue::cause(value)]; optional[]; } identity { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "identity"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error source_unit_id_invalid_resolver; imports { use super:: { Display,
        ErrorValue, IntoRiftError, SetState }; }
        metadata["rift.core.source_unit_id_invalid_resolver",
        "source-unit resolver identity is invalid: {cause}",
        "correct the reported field and resend the request", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { cause { imports {
        use super:: { Builder, ErrorValue, IntoRiftError, SetState }; } output[SetState,
        State1]; index 0u32; key "cause"; flags[true, false]; bound[IntoRiftError]; value
        value => [ErrorValue::cause(value)]; optional[]; } identity { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "identity"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error source_unit_id_non_canonical; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.source_unit_id_non_canonical",
        "source-unit address is not in canonical form",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { identity { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "identity"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error source_unit_id_too_long; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.core.source_unit_id_too_long",
        "source-unit identity exceeds its protocol byte limit",
        "correct the reported field and resend the request", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { identity { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "identity"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
}
#[allow(missing_docs)]
pub mod history {
    use super::{
        Display, ErrorValue, IntoUnsigned, Path, SetState, __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error blob_too_large; imports { use super:: { ErrorValue, IntoUnsigned, Path,
        SetState }; } metadata["rift.history.blob_too_large",
        "committed file {path} has {size} bytes, above accepted limit {bytes_max}",
        "use a revision with a smaller file", 3usize]; builder Builder; states[State0,
        State1, State2]; complete[SetState, SetState, SetState]; fields { bytes_max {
        imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "bytes_max"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } path { imports { use super:: { Builder, ErrorValue, Path, SetState
        }; } output[State0, SetState, State2]; index 1u32; key "path"; flags[true,
        false]; bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[]; } size { imports { use super:: { Builder, ErrorValue, IntoUnsigned,
        SetState }; } output[State0, State1, SetState]; index 2u32; key "size";
        flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error contribution_invalid; imports { use super:: { Display, ErrorValue, SetState
        }; } metadata["rift.history.contribution_invalid",
        "history Contribution conversion rejected: {detail}",
        "check the history Contribution data and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { detail { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "detail"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error path_unrepresentable; imports { use super:: { Display, ErrorValue, SetState
        }; } metadata["rift.history.path_unrepresentable",
        "committed path cannot be represented as UTF-8: {path}",
        "rename the committed path to valid UTF-8 and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { path { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "path"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error revision_not_commit; imports { use super:: { Display, ErrorValue, SetState
        }; } metadata["rift.history.revision_not_commit",
        "revision {rev} resolves to {resolved_kind}, not a commit",
        "use a revision that names a commit", 3usize]; builder Builder; states[State0,
        State1, State2]; complete[SetState, SetState, SetState]; fields { requires {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "requires"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; }
        resolved_kind { imports { use super:: { Builder, Display, ErrorValue, SetState };
        } output[State0, SetState, State2]; index 1u32; key "resolved_kind"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } rev { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, State1, SetState]; index 2u32; key "rev"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error revision_unknown; imports { use super:: { Display, ErrorValue, SetState };
        } metadata["rift.history.revision_unknown", "revision does not resolve: {rev}",
        "use a branch, tag, or commit id this repository resolves", 2usize]; builder
        Builder; states[State0, State1]; complete[SetState, SetState]; fields { requires
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1]; index 0u32; key "requires"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } rev {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "rev"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error storage; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.history.storage",
        "repository storage failed during {operation}: {detail}",
        "check repository storage and retry", 2usize]; builder Builder; states[State0,
        State1]; complete[SetState, SetState]; fields { detail { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState, State1]; index 0u32;
        key "detail"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } operation { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[State0, SetState]; index 1u32;
        key "operation"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error too_many_tags; imports { use super:: { Display, ErrorValue, IntoUnsigned,
        SetState }; } metadata["rift.history.too_many_tags",
        "repository tag count exceeds accepted limit {tags_max}",
        "reduce repository tags or raise the tag limit", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { limit { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "limit"; flags[true, false]; bound[Display]; value value
        => [ErrorValue::display(value)]; optional[]; } tags_max { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, SetState]; index
        1u32; key "tags_max"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error tree_too_large; imports { use super:: { Display, ErrorValue, IntoUnsigned,
        SetState }; } metadata["rift.history.tree_too_large",
        "revision tree exceeds accepted entry limit {entries_max}",
        "use a revision with a smaller tree", 2usize]; builder Builder; states[State0,
        State1]; complete[SetState, SetState]; fields { entries_max { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState,
        State1]; index 0u32; key "entries_max"; flags[true, false]; bound[IntoUnsigned];
        value value => [ErrorValue::unsigned(value)]; optional[]; } limit { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "limit"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error unversioned; imports { use super:: { Display, ErrorValue, Path, SetState };
        } metadata["rift.history.unversioned",
        "workspace has no git repository: {workspace}",
        "run `git init`, or omit `rev` to read current tree", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { requires { imports
        { use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "requires"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } workspace { imports { use
        super:: { Builder, ErrorValue, Path, SetState }; } output[State0, SetState];
        index 1u32; key "workspace"; flags[true, false]; bound[AsRef < Path >]; value
        value => [ErrorValue::path(value)]; optional[]; } }
    }
}
#[allow(missing_docs)]
pub mod history_store {
    use super::{
        Box, Display, Error, ErrorValue, IntoUnsigned, Path, SetState,
        __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error database; imports { use super:: { Box, Display, Error, ErrorValue, SetState
        }; } metadata["rift.history_store.database",
        "history store database operation failed: {operation}: {detail}",
        "check the history store database and retry", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { detail { imports {
        use super:: { Box, Builder, Error, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "detail"; flags[true, false]; bound[Into < Box < dyn
        Error + Send + Sync + 'static >>]; value value => [ErrorValue::source(value)];
        optional[]; } operation { imports { use super:: { Builder, Display, ErrorValue,
        SetState }; } output[State0, SetState]; index 1u32; key "operation"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } }
    }
    __rift_error_definition! {
        error folder; imports { use super:: { Box, Display, Error, ErrorValue, Path,
        SetState }; } metadata["rift.history_store.folder",
        "history store folder operation failed: {operation} {path}: {detail}",
        "check the history store path and permissions", 3usize]; builder Builder;
        states[State0, State1, State2]; complete[SetState, SetState, SetState]; fields {
        detail { imports { use super:: { Box, Builder, Error, ErrorValue, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "detail"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } operation { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[State0, SetState, State2];
        index 1u32; key "operation"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } path { imports { use super:: {
        Builder, ErrorValue, Path, SetState }; } output[State0, State1, SetState]; index
        2u32; key "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error lock_unstable; imports { use super:: { ErrorValue, IntoUnsigned, Path,
        SetState }; } metadata["rift.history_store.lock_unstable",
        "history store live lock changed during {attempts} attempts: {path}",
        "retry after concurrent history store cleanup finishes", 2usize]; builder
        Builder; states[State0, State1]; complete[SetState, SetState]; fields { attempts
        { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[SetState, State1]; index 0u32; key "attempts"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        path { imports { use super:: { Builder, ErrorValue, Path, SetState }; }
        output[State0, SetState]; index 1u32; key "path"; flags[true, false]; bound[AsRef
        < Path >]; value value => [ErrorValue::path(value)]; optional[]; } }
    }
}
#[allow(missing_docs)]
pub mod index {
    use super::{
        Box, Display, Error, ErrorValue, IntoRiftError, IntoUnsigned, Path, SetState,
        __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error lexical_document_location_unsupported; imports { use super:: { ErrorValue,
        Path }; } metadata["rift.index.lexical_document_location_unsupported",
        "lexical index cannot store this document location",
        "store package documents in the global index", 1usize]; builder Builder;
        states[]; complete[]; fields { path { imports { use super:: { Builder,
        ErrorValue, Path }; } output[]; index 0u32; key "path"; flags[true, false];
        bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error lexical_duplicate_identity; imports { use super:: { ErrorValue, Path }; }
        metadata["rift.index.lexical_duplicate_identity",
        "lexical index received a repeated document identity",
        "send each document identity once and retry", 1usize]; builder Builder; states[];
        complete[]; fields { path { imports { use super:: { Builder, ErrorValue, Path };
        } output[]; index 0u32; key "path"; flags[true, false]; bound[AsRef < Path >];
        value value => [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error lexical_record_limit; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.index.lexical_record_limit",
        "lexical index received more records than its accepted limit of {maximum}",
        "reduce records below {maximum} and retry", 3usize]; builder Builder;
        states[State0, State1, State2]; complete[SetState, SetState, SetState]; fields {
        field { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; }
        maximum { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState };
        } output[State0, SetState, State2]; index 1u32; key "maximum"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } observed { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned, SetState }; } output[State0, State1, SetState]; index 2u32; key
        "observed"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error lexical_storage; imports { use super:: { Box, Error, ErrorValue, Path }; }
        metadata["rift.index.lexical_storage", "lexical index operation failed",
        "check filesystem permissions and free space, then retry", 2usize]; builder
        Builder; states[]; complete[]; fields { path { imports { use super:: { Builder,
        ErrorValue, Path }; } output[]; index 0u32; key "path"; flags[true, false];
        bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[maybe_path]; } source { imports { use super:: { Box, Builder, Error,
        ErrorValue }; } output[]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error lexical_stored_kind_invalid; imports { use super:: { Box, Error,
        ErrorValue, Path }; } metadata["rift.index.lexical_stored_kind_invalid",
        "stored document kind is invalid", "repair the indexed document kind and retry",
        2usize]; builder Builder; states[]; complete[]; fields { path { imports { use
        super:: { Builder, ErrorValue, Path }; } output[]; index 0u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } source { imports { use super::
        { Box, Builder, Error, ErrorValue }; } output[]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error lexical_stored_path_invalid; imports { use super:: { Box, Error,
        ErrorValue, Path }; } metadata["rift.index.lexical_stored_path_invalid",
        "stored project path is invalid", "repair the indexed path and retry", 2usize];
        builder Builder; states[]; complete[]; fields { path { imports { use super:: {
        Builder, ErrorValue, Path }; } output[]; index 0u32; key "path"; flags[true,
        false]; bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[maybe_path]; } source { imports { use super:: { Box, Builder, Error,
        ErrorValue }; } output[]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error lexical_unit_limit; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.index.lexical_unit_limit",
        "lexical index received more units than its accepted limit of {maximum}",
        "reduce indexed units below {maximum} and retry", 3usize]; builder Builder;
        states[State0, State1, State2]; complete[SetState, SetState, SetState]; fields {
        field { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; }
        maximum { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState };
        } output[State0, SetState, State2]; index 1u32; key "maximum"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } observed { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned, SetState }; } output[State0, State1, SetState]; index 2u32; key
        "observed"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error lexical_unit_too_large; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, Path, SetState }; } metadata["rift.index.lexical_unit_too_large",
        "document content exceeds its accepted byte limit of {maximum}",
        "shorten document content below {maximum} bytes and retry", 4usize]; builder
        Builder; states[State0, State1, State2]; complete[SetState, SetState, SetState];
        fields { field { imports { use super:: { Builder, Display, ErrorValue, SetState
        }; } output[SetState, State1, State2]; index 0u32; key "field"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } maximum { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState
        }; } output[State0, SetState, State2]; index 1u32; key "maximum"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } observed { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned, SetState }; } output[State0, State1, SetState]; index 2u32; key
        "observed"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } path { imports { use super:: {
        Builder, ErrorValue, Path }; } output[State0, State1, State2]; index 3u32; key
        "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error workspace_cancelled; imports { use super:: { ErrorValue, Path }; }
        metadata["rift.index.workspace_cancelled", "workspace indexing was cancelled",
        "retry workspace indexing", 1usize]; builder Builder; states[]; complete[];
        fields { path { imports { use super:: { Builder, ErrorValue, Path }; } output[];
        index 0u32; key "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error workspace_changed_during_capture; imports { use super:: { Box, Error,
        ErrorValue, Path }; } metadata["rift.index.workspace_changed_during_capture",
        "source file changed while its bytes were captured", "retry workspace indexing",
        2usize]; builder Builder; states[]; complete[]; fields { path { imports { use
        super:: { Builder, ErrorValue, Path }; } output[]; index 0u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } source { imports { use super::
        { Box, Builder, Error, ErrorValue }; } output[]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error workspace_composition; imports { use super:: { ErrorValue, IntoRiftError };
        } metadata["rift.index.workspace_composition",
        "workspace composition failed validation",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[]; complete[]; fields { cause { imports { use super:: { Builder,
        ErrorValue, IntoRiftError }; } output[]; index 0u32; key "cause"; flags[true,
        false]; bound[IntoRiftError]; value value => [ErrorValue::cause(value)];
        optional[maybe_cause]; } }
    }
    __rift_error_definition! {
        error workspace_documentation_limit; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, Path, SetState }; }
        metadata["rift.index.workspace_documentation_limit",
        "workspace documentation sources exceed their accepted limit of {maximum}",
        "reduce documentation sources below {maximum} and retry", 4usize]; builder
        Builder; states[State0, State1, State2]; complete[SetState, SetState, SetState];
        fields { field { imports { use super:: { Builder, Display, ErrorValue, SetState
        }; } output[SetState, State1, State2]; index 0u32; key "field"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } maximum { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState
        }; } output[State0, SetState, State2]; index 1u32; key "maximum"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } observed { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned, SetState }; } output[State0, State1, SetState]; index 2u32; key
        "observed"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } path { imports { use super:: {
        Builder, ErrorValue, Path }; } output[State0, State1, State2]; index 3u32; key
        "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error workspace_file_too_large; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, Path }; } metadata["rift.index.workspace_file_too_large",
        "source file exceeds its accepted byte limit of {maximum}",
        "reduce source file size below {maximum} bytes and retry", 4usize]; builder
        Builder; states[]; complete[]; fields { field { imports { use super:: { Builder,
        Display, ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } maximum { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned }; } output[]; index 1u32; key "maximum"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[maybe_maximum]; } observed { imports { use super:: { Builder,
        ErrorValue, IntoUnsigned }; } output[]; index 2u32; key "observed"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[maybe_observed]; } path { imports { use super:: { Builder, ErrorValue,
        Path }; } output[]; index 3u32; key "path"; flags[true, false]; bound[AsRef <
        Path >]; value value => [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error workspace_filesystem; imports { use super:: { Box, Error, ErrorValue, Path
        }; } metadata["rift.index.workspace_filesystem",
        "workspace filesystem operation failed",
        "check filesystem permissions and free space, then retry", 2usize]; builder
        Builder; states[]; complete[]; fields { path { imports { use super:: { Builder,
        ErrorValue, Path }; } output[]; index 0u32; key "path"; flags[true, false];
        bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[maybe_path]; } source { imports { use super:: { Box, Builder, Error,
        ErrorValue }; } output[]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error workspace_history; imports { use super:: { ErrorValue, IntoRiftError, Path
        }; } metadata["rift.index.workspace_history",
        "workspace history operation failed", "check repository state and retry",
        2usize]; builder Builder; states[]; complete[]; fields { cause { imports { use
        super:: { Builder, ErrorValue, IntoRiftError }; } output[]; index 0u32; key
        "cause"; flags[true, false]; bound[IntoRiftError]; value value =>
        [ErrorValue::cause(value)]; optional[maybe_cause]; } path { imports { use super::
        { Builder, ErrorValue, Path }; } output[]; index 1u32; key "path"; flags[true,
        false]; bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error workspace_invalid_path; imports { use super:: { Box, Error, ErrorValue,
        IntoRiftError, Path }; } metadata["rift.index.workspace_invalid_path",
        "workspace path is not valid project syntax",
        "use a canonical project path and retry", 3usize]; builder Builder; states[];
        complete[]; fields { cause { imports { use super:: { Builder, ErrorValue,
        IntoRiftError }; } output[]; index 0u32; key "cause"; flags[true, false];
        bound[IntoRiftError]; value value => [ErrorValue::cause(value)];
        optional[maybe_cause]; } path { imports { use super:: { Builder, ErrorValue, Path
        }; } output[]; index 1u32; key "path"; flags[true, false]; bound[AsRef < Path >];
        value value => [ErrorValue::path(value)]; optional[maybe_path]; } source {
        imports { use super:: { Box, Builder, Error, ErrorValue }; } output[]; index
        2u32; key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send +
        Sync + 'static >>]; value value => [ErrorValue::source(value)];
        optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error workspace_invalid_root; imports { use super:: { Box, Error, ErrorValue,
        Path }; } metadata["rift.index.workspace_invalid_root",
        "workspace root cannot be read as a directory",
        "provide a readable workspace directory and retry", 2usize]; builder Builder;
        states[]; complete[]; fields { path { imports { use super:: { Builder,
        ErrorValue, Path }; } output[]; index 0u32; key "path"; flags[true, false];
        bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[maybe_path]; } source { imports { use super:: { Box, Builder, Error,
        ErrorValue }; } output[]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error workspace_invalid_source; imports { use super:: { Box, Error, ErrorValue,
        Path }; } metadata["rift.index.workspace_invalid_source",
        "source file bytes are not valid UTF-8", "save source file as UTF-8 and retry",
        2usize]; builder Builder; states[]; complete[]; fields { path { imports { use
        super:: { Builder, ErrorValue, Path }; } output[]; index 0u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } source { imports { use super::
        { Box, Builder, Error, ErrorValue }; } output[]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error workspace_language_include_required; imports { use super:: { Display,
        ErrorValue }; } metadata["rift.index.workspace_language_include_required",
        "unshipped language has no nonempty include list",
        "add a nonempty include list for this language and retry", 1usize]; builder
        Builder; states[]; complete[]; fields { language { imports { use super:: {
        Builder, Display, ErrorValue }; } output[]; index 0u32; key "language";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_language]; } }
    }
    __rift_error_definition! {
        error workspace_language_match_conflict; imports { use super:: { Display,
        ErrorValue, Path }; } metadata["rift.index.workspace_language_match_conflict",
        "workspace path matches two language entries",
        "make language include lists distinct and retry", 2usize]; builder Builder;
        states[]; complete[]; fields { language { imports { use super:: { Builder,
        Display, ErrorValue }; } output[]; index 0u32; key "language"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_language]; } path { imports { use super:: { Builder, ErrorValue,
        Path }; } output[]; index 1u32; key "path"; flags[true, false]; bound[AsRef <
        Path >]; value value => [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error workspace_provider; imports { use super:: { Box, Error, ErrorValue,
        IntoRiftError, Path }; } metadata["rift.index.workspace_provider",
        "provider publication failed",
        "report this internal failure with its full context", 3usize]; builder Builder;
        states[]; complete[]; fields { cause { imports { use super:: { Builder,
        ErrorValue, IntoRiftError }; } output[]; index 0u32; key "cause"; flags[true,
        false]; bound[IntoRiftError]; value value => [ErrorValue::cause(value)];
        optional[maybe_cause]; } path { imports { use super:: { Builder, ErrorValue, Path
        }; } output[]; index 1u32; key "path"; flags[true, false]; bound[AsRef < Path >];
        value value => [ErrorValue::path(value)]; optional[maybe_path]; } source {
        imports { use super:: { Box, Builder, Error, ErrorValue }; } output[]; index
        2u32; key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send +
        Sync + 'static >>]; value value => [ErrorValue::source(value)];
        optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error workspace_result_limit; imports { use super:: { Display, ErrorValue,
        IntoUnsigned }; } metadata["rift.index.workspace_result_limit",
        "workspace search returned more results than its accepted limit of {maximum}",
        "reduce requested results below {maximum} and retry", 3usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } maximum { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned }; } output[]; index 1u32; key "maximum"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[maybe_maximum]; } observed { imports { use super:: { Builder,
        ErrorValue, IntoUnsigned }; } output[]; index 2u32; key "observed"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[maybe_observed]; } }
    }
    __rift_error_definition! {
        error workspace_syntax; imports { use super:: { Box, Error, ErrorValue,
        IntoRiftError, Path }; } metadata["rift.index.workspace_syntax",
        "Rust syntax analysis failed", "correct the source syntax and retry", 3usize];
        builder Builder; states[]; complete[]; fields { cause { imports { use super:: {
        Builder, ErrorValue, IntoRiftError }; } output[]; index 0u32; key "cause";
        flags[true, false]; bound[IntoRiftError]; value value =>
        [ErrorValue::cause(value)]; optional[maybe_cause]; } path { imports { use super::
        { Builder, ErrorValue, Path }; } output[]; index 1u32; key "path"; flags[true,
        false]; bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[maybe_path]; } source { imports { use super:: { Box, Builder, Error,
        ErrorValue }; } output[]; index 2u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error workspace_too_deep; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, Path, SetState }; } metadata["rift.index.workspace_too_deep",
        "workspace directory depth exceeds its accepted limit of {maximum}",
        "reduce directory depth below {maximum} and retry", 4usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue }; } output[State0]; index 0u32; key "field";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } maximum { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned, SetState }; } output[SetState]; index 1u32; key "maximum";
        flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } observed { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned }; } output[State0]; index 2u32; key
        "observed"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[maybe_observed]; } path { imports { use
        super:: { Builder, ErrorValue, Path }; } output[State0]; index 3u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error workspace_too_many_files; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, Path }; } metadata["rift.index.workspace_too_many_files",
        "workspace contains more files than its accepted limit of {maximum}",
        "reduce workspace files below {maximum} and retry", 4usize]; builder Builder;
        states[]; complete[]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } maximum { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned }; } output[]; index 1u32; key "maximum"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[maybe_maximum]; } observed { imports { use super:: { Builder,
        ErrorValue, IntoUnsigned }; } output[]; index 2u32; key "observed"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[maybe_observed]; } path { imports { use super:: { Builder, ErrorValue,
        Path }; } output[]; index 3u32; key "path"; flags[true, false]; bound[AsRef <
        Path >]; value value => [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error workspace_workspace_too_large; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, Path }; } metadata["rift.index.workspace_workspace_too_large",
        "workspace source bytes exceed their accepted limit of {maximum}",
        "reduce workspace source bytes below {maximum} and retry", 4usize]; builder
        Builder; states[]; complete[]; fields { field { imports { use super:: { Builder,
        Display, ErrorValue }; } output[]; index 0u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } maximum { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned }; } output[]; index 1u32; key "maximum"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[maybe_maximum]; } observed { imports { use super:: { Builder,
        ErrorValue, IntoUnsigned }; } output[]; index 2u32; key "observed"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[maybe_observed]; } path { imports { use super:: { Builder, ErrorValue,
        Path }; } output[]; index 3u32; key "path"; flags[true, false]; bound[AsRef <
        Path >]; value value => [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error workspace_zero_limit; imports { use super:: { Display, ErrorValue,
        IntoRiftError }; } metadata["rift.index.workspace_zero_limit",
        "workspace index bound is zero", "set every workspace index bound above zero",
        2usize]; builder Builder; states[]; complete[]; fields { cause { imports { use
        super:: { Builder, ErrorValue, IntoRiftError }; } output[]; index 0u32; key
        "cause"; flags[true, false]; bound[IntoRiftError]; value value =>
        [ErrorValue::cause(value)]; optional[maybe_cause]; } field { imports { use
        super:: { Builder, Display, ErrorValue }; } output[]; index 1u32; key "field";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } }
    }
}
#[allow(missing_docs)]
pub mod lsp {
    use super::{
        Box, Display, Error, ErrorValue, IntoInteger, IntoUnsigned, SetState,
        __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error capabilities_position_encoding_unsupported; imports { use super:: {
        Display, ErrorValue, SetState }; }
        metadata["rift.lsp.capabilities_position_encoding_unsupported",
        "language engine selected unsupported position encoding {encoding}",
        "configure the engine to use UTF-8 or UTF-16 positions", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { encoding { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "encoding"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error correlation_pending_requests_exceeded; imports { use super:: { Display,
        ErrorValue, IntoUnsigned, SetState }; }
        metadata["rift.lsp.correlation_pending_requests_exceeded",
        "language engine has more than {limit} pending requests",
        "wait for a pending request to finish before sending another", 3usize]; builder
        Builder; states[State0, State1, State2]; complete[SetState, SetState, SetState];
        fields { field { imports { use super:: { Builder, Display, ErrorValue, SetState
        }; } output[SetState, State1, State2]; index 0u32; key "field"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } limit { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState };
        } output[State0, SetState, State2]; index 1u32; key "limit"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        required { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState };
        } output[State0, State1, SetState]; index 2u32; key "required"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } }
    }
    __rift_error_definition! {
        error correlation_response_unknown; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.lsp.correlation_response_unknown",
        "language engine answered unknown request id {id}",
        "check the language engine's request and response correlation", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { id { imports { use super::
        { Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "id"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error engine_analyzing; imports { use super:: { ErrorValue, IntoUnsigned,
        SetState }; } metadata["rift.lsp.engine_analyzing",
        "language engine remained analyzing after {attempts} attempts",
        "wait for language engine analysis to finish and retry", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { attempts { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState];
        index 0u32; key "attempts"; flags[true, false]; bound[IntoUnsigned]; value value
        => [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error engine_capability_absent; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.lsp.engine_capability_absent",
        "language engine does not advertise {capability}",
        "configure an engine that advertises {capability}", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { capability { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "capability"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error engine_connection_closed; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.lsp.engine_connection_closed",
        "language engine closed connection during {method}",
        "restart the language engine and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { method { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "method"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error engine_ended; imports {} metadata["rift.lsp.engine_ended",
        "language engine session has ended", "start a new language engine session",
        0usize]; builder Builder; states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error engine_launch_failed; imports { use super:: { Box, Error, ErrorValue,
        SetState }; } metadata["rift.lsp.engine_launch_failed",
        "language engine could not start",
        "check the configured program and process limits", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { source { imports { use super:: {
        Box, Builder, Error, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error engine_message_unreadable; imports {}
        metadata["rift.lsp.engine_message_unreadable",
        "language engine response is not a JSON-RPC envelope",
        "check the language engine's LSP response", 0usize]; builder Builder; states[];
        complete[]; fields {}
    }
    __rift_error_definition! {
        error engine_program_absolute; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.lsp.engine_program_absolute",
        "language engine program {program} is an absolute path",
        "set a program name resolved from the inherited PATH", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { program { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "program"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error engine_program_empty; imports {} metadata["rift.lsp.engine_program_empty",
        "language engine program is empty", "set a program for the language engine",
        0usize]; builder Builder; states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error engine_refused_retryable; imports { use super:: { Display, ErrorValue,
        IntoInteger, SetState }; } metadata["rift.lsp.engine_refused_retryable",
        "language engine refused {method} with code {code}: {message}",
        "resend the same request after the engine finishes its current work", 3usize];
        builder Builder; states[State0, State1, State2]; complete[SetState, SetState,
        SetState]; fields { code { imports { use super:: { Builder, ErrorValue,
        IntoInteger, SetState }; } output[SetState, State1, State2]; index 0u32; key
        "code"; flags[true, false]; bound[IntoInteger]; value value =>
        [ErrorValue::integer(value)]; optional[]; } message { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[State0, SetState, State2];
        index 1u32; key "message"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } method { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[State0, State1, SetState];
        index 2u32; key "method"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error engine_refused_terminal; imports { use super:: { Display, ErrorValue,
        IntoInteger, SetState }; } metadata["rift.lsp.engine_refused_terminal",
        "language engine refused {method} with code {code}: {message}",
        "correct the request and retry", 3usize]; builder Builder; states[State0, State1,
        State2]; complete[SetState, SetState, SetState]; fields { code { imports { use
        super:: { Builder, ErrorValue, IntoInteger, SetState }; } output[SetState,
        State1, State2]; index 0u32; key "code"; flags[true, false]; bound[IntoInteger];
        value value => [ErrorValue::integer(value)]; optional[]; } message { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[State0,
        SetState, State2]; index 1u32; key "message"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } method { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, State1,
        SetState]; index 2u32; key "method"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error engine_result_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.lsp.engine_result_invalid",
        "language engine response for {method} has an invalid result",
        "check the language engine's response for {method}", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { method { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "method"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } source { imports { use
        super:: { Box, Builder, Error, ErrorValue, SetState }; } output[State0,
        SetState]; index 1u32; key "source"; flags[true, false]; bound[Into < Box < dyn
        Error + Send + Sync + 'static >>]; value value => [ErrorValue::source(value)];
        optional[]; } }
    }
    __rift_error_definition! {
        error engine_timed_out; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.lsp.engine_timed_out",
        "language engine timed out during {method} after {timeout_ms} ms",
        "retry the request after checking language engine load", 2usize]; builder
        Builder; states[State0, State1]; complete[SetState, SetState]; fields { method {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1]; index 0u32; key "method"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; }
        timeout_ms { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState
        }; } output[State0, SetState]; index 1u32; key "timeout_ms"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        }
    }
    __rift_error_definition! {
        error framing_content_length_invalid; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.lsp.framing_content_length_invalid",
        "language engine message has invalid Content-Length {value}",
        "send Content-Length as a decimal byte count", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { value { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "value"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error framing_content_length_missing; imports {}
        metadata["rift.lsp.framing_content_length_missing",
        "language engine message header has no Content-Length",
        "check the language engine's LSP framing and retry", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error framing_header_malformed; imports {}
        metadata["rift.lsp.framing_header_malformed",
        "language engine message header is malformed",
        "check the language engine's LSP framing and retry", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error framing_header_too_long; imports {}
        metadata["rift.lsp.framing_header_too_long",
        "language engine message header exceeds its byte limit",
        "reduce the message header and retry", 0usize]; builder Builder; states[];
        complete[]; fields {}
    }
    __rift_error_definition! {
        error framing_message_too_long; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.lsp.framing_message_too_long",
        "language engine message body of {announced_bytes} bytes exceeds limit {limit}",
        "reduce the message body below {limit} bytes and retry", 4usize]; builder
        Builder; states[State0, State1, State2, State3]; complete[SetState, SetState,
        SetState, SetState]; fields { announced_bytes { imports { use super:: { Builder,
        ErrorValue, IntoUnsigned, SetState }; } output[SetState, State1, State2, State3];
        index 0u32; key "announced_bytes"; flags[true, false]; bound[IntoUnsigned]; value
        value => [ErrorValue::unsigned(value)]; optional[]; } field { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState,
        State2, State3]; index 1u32; key "field"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } limit { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, State1,
        SetState, State3]; index 2u32; key "limit"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        required { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState };
        } output[State0, State1, State2, SetState]; index 3u32; key "required";
        flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error position_character_misaligned; imports { use super:: { ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.lsp.position_character_misaligned",
        "character {character} splits an encoded character on line {line}",
        "use a character offset on an encoding boundary", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { character {
        imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[SetState, State1]; index 0u32; key "character"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        line { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[State0, SetState]; index 1u32; key "line"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        }
    }
    __rift_error_definition! {
        error position_character_out_of_range; imports { use super:: { ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.lsp.position_character_out_of_range",
        "character {character} is outside line {line} length {line_units}",
        "use a character offset within the line", 3usize]; builder Builder;
        states[State0, State1, State2]; complete[SetState, SetState, SetState]; fields {
        character { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState
        }; } output[SetState, State1, State2]; index 0u32; key "character"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } line { imports { use super:: { Builder, ErrorValue, IntoUnsigned,
        SetState }; } output[State0, SetState, State2]; index 1u32; key "line";
        flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } line_units { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, State1,
        SetState]; index 2u32; key "line_units"; flags[true, false]; bound[IntoUnsigned];
        value value => [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error position_line_out_of_range; imports { use super:: { ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.lsp.position_line_out_of_range",
        "position line {line} is outside document line count {line_count}",
        "use a line within the document", 2usize]; builder Builder; states[State0,
        State1]; complete[SetState, SetState]; fields { line { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState, State1]; index
        0u32; key "line"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } line_count { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, SetState]; index
        1u32; key "line_count"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error position_offset_inside_line_ending; imports { use super:: { ErrorValue,
        IntoUnsigned, SetState }; }
        metadata["rift.lsp.position_offset_inside_line_ending",
        "byte offset {byte_offset} falls inside a line ending",
        "use a byte offset before or after the line ending", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { byte_offset { imports { use super::
        { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState]; index 0u32;
        key "byte_offset"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error position_offset_misaligned; imports { use super:: { ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.lsp.position_offset_misaligned",
        "byte offset {byte_offset} splits a UTF-8 character",
        "use a byte offset on a UTF-8 boundary", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { byte_offset { imports { use super::
        { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState]; index 0u32;
        key "byte_offset"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error position_offset_out_of_range; imports { use super:: { ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.lsp.position_offset_out_of_range",
        "byte offset {byte_offset} exceeds document size {document_bytes}",
        "use a byte offset within the document", 2usize]; builder Builder; states[State0,
        State1]; complete[SetState, SetState]; fields { byte_offset { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState,
        State1]; index 0u32; key "byte_offset"; flags[true, false]; bound[IntoUnsigned];
        value value => [ErrorValue::unsigned(value)]; optional[]; } document_bytes {
        imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[State0, SetState]; index 1u32; key "document_bytes"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        }
    }
    __rift_error_definition! {
        error uri_host_refused; imports { use super:: { Display, ErrorValue, SetState };
        } metadata["rift.lsp.uri_host_refused",
        "document URI host {host} is not supported",
        "use a hostless file URI for the document", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { host { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "host"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error uri_outside_root; imports {} metadata["rift.lsp.uri_outside_root",
        "document URI is outside the workspace root",
        "use a document URI under the workspace root", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error uri_path_not_decodable; imports {}
        metadata["rift.lsp.uri_path_not_decodable",
        "document URI path does not decode to Unicode",
        "use a file URI with a UTF-8 path", 0usize]; builder Builder; states[];
        complete[]; fields {}
    }
    __rift_error_definition! {
        error uri_root_not_absolute; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.lsp.uri_root_not_absolute",
        "language engine workspace root {root} is not absolute",
        "configure an absolute workspace root", 1usize]; builder Builder; states[State0];
        complete[SetState]; fields { root { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState]; index 0u32; key "root"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } }
    }
    __rift_error_definition! {
        error uri_root_not_unicode; imports {} metadata["rift.lsp.uri_root_not_unicode",
        "language engine workspace root is not valid Unicode",
        "use a workspace root with a Unicode path", 0usize]; builder Builder; states[];
        complete[]; fields {}
    }
    __rift_error_definition! {
        error uri_scheme_refused; imports { use super:: { Display, ErrorValue, SetState
        }; } metadata["rift.lsp.uri_scheme_refused",
        "document URI scheme {scheme} is not supported",
        "use a file URI for the document", 1usize]; builder Builder; states[State0];
        complete[SetState]; fields { scheme { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState]; index 0u32; key "scheme"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } }
    }
    __rift_error_definition! {
        error uri_uri_malformed; imports { use super:: { Display, ErrorValue, SetState };
        } metadata["rift.lsp.uri_uri_malformed",
        "language engine returned malformed document URI {uri}",
        "use a valid file URI for the document", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { uri { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "uri"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
}
#[allow(missing_docs)]
pub mod mcp {
    use super::{
        Borrow, Box, Display, Duration, Error, ErrorValue, IntoRiftError, IntoUnsigned,
        Path, SetState, __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error answer_structure_failed; imports { use super:: { Box, Error, ErrorValue,
        SetState }; } metadata["rift.mcp.answer_structure_failed",
        "answer could not be serialized into structured content: {source}",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { source { imports { use super:: {
        Box, Builder, Error, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error answer_text_failed; imports { use super:: { Box, Error, ErrorValue,
        SetState }; } metadata["rift.mcp.answer_text_failed",
        "answer text could not be written: {source}",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { source { imports { use super:: {
        Box, Builder, Error, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error answer_text_limit; imports { use super:: { ErrorValue, IntoUnsigned,
        SetState }; } metadata["rift.mcp.answer_text_limit",
        "answer text exceeds its accepted limit of {limit} bytes",
        "narrow the request or lower `limit`, then resend the request", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { limit { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState];
        index 0u32; key "limit"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error arguments_not_object; imports { use super:: { Display, ErrorValue, SetState
        }; } metadata["rift.mcp.arguments_not_object",
        "tool call arguments are not a JSON object",
        "send tool call arguments as a JSON object and resend the request", 1usize];
        builder Builder; states[State0]; complete[SetState]; fields { field { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error election_already_serving; imports {}
        metadata["rift.mcp.election_already_serving",
        "another process already serves this workspace",
        "connect to the serving process or stop it before starting another", 0usize];
        builder Builder; states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error election_document_invalid; imports {}
        metadata["rift.mcp.election_document_invalid",
        "server lock document failed validation",
        "report this internal failure with its full context", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error election_storage_failed; imports { use super:: { Box, Display, Error,
        ErrorValue, Path, SetState }; } metadata["rift.mcp.election_storage_failed",
        "workspace server election state could not be read or written",
        "check filesystem permissions and free space, then retry", 3usize]; builder
        Builder; states[State0, State1, State2]; complete[SetState, SetState, SetState];
        fields { operation { imports { use super:: { Builder, Display, ErrorValue,
        SetState }; } output[SetState, State1, State2]; index 0u32; key "operation";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[]; } path { imports { use super:: { Builder, ErrorValue, Path, SetState
        }; } output[State0, SetState, State2]; index 1u32; key "path"; flags[true,
        false]; bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[]; } source { imports { use super:: { Box, Builder, Error, ErrorValue,
        SetState }; } output[State0, State1, SetState]; index 2u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error forward_unanswered; imports { use super:: { Borrow, Duration, ErrorValue,
        SetState }; } metadata["rift.mcp.forward_unanswered",
        "workspace server did not answer forwarded request within {waited}",
        "resend the same request after a short delay", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { waited { imports { use super:: {
        Borrow, Builder, Duration, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "waited"; flags[true, false]; bound[Borrow < Duration >]; value value
        => [ErrorValue::duration(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error http_ports_exhausted; imports { use super:: { Borrow, ErrorValue, SetState
        }; } metadata["rift.mcp.http_ports_exhausted",
        "every loopback port in the serving range is bound",
        "stop a process using a serving port, then retry", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { port_max { imports
        { use super:: { Borrow, Builder, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "port_max"; flags[true, false]; bound[Borrow < u16 >];
        value value => [ErrorValue::port(value)]; optional[]; } port_min { imports { use
        super:: { Borrow, Builder, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "port_min"; flags[true, false]; bound[Borrow < u16 >]; value
        value => [ErrorValue::port(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error http_serve_failed; imports { use super:: { Box, Display, Error, ErrorValue,
        IntoRiftError, SetState }; } metadata["rift.mcp.http_serve_failed",
        "HTTP MCP server failed while serving",
        "report this internal failure with its full context", 3usize]; builder Builder;
        states[State0]; complete[SetState]; fields { cause { imports { use super:: {
        Builder, ErrorValue, IntoRiftError }; } output[State0]; index 0u32; key "cause";
        flags[true, false]; bound[IntoRiftError]; value value =>
        [ErrorValue::cause(value)]; optional[maybe_cause]; } operation { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        1u32; key "operation"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0]; index 2u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error parameter_invalid; imports { use super:: { Display, ErrorValue, SetState };
        } metadata["rift.mcp.parameter_invalid",
        "tool arguments do not match the served schema",
        "correct the reported field and resend the request", 4usize]; builder Builder;
        states[State0]; complete[SetState]; fields { accepted { imports { use super:: {
        Builder, Display, ErrorValue }; } output[State0]; index 0u32; key "accepted";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_accepted]; } example { imports { use super:: { Builder, Display,
        ErrorValue }; } output[State0]; index 1u32; key "example"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_example]; } field { imports { use super:: { Builder, Display,
        ErrorValue }; } output[State0]; index 2u32; key "field"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_field]; } tool { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState]; index 3u32; key "tool"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } }
    }
    __rift_error_definition! {
        error project_hit_identity_missing; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.mcp.project_hit_identity_missing",
        "project hit has no identity",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { hit { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "hit"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error project_hit_identity_refused; imports { use super:: { Display, ErrorValue,
        IntoRiftError, SetState }; } metadata["rift.mcp.project_hit_identity_refused",
        "project hit identity was refused by ranking",
        "report this internal failure with its full context", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { cause { imports { use super:: {
        Builder, ErrorValue, IntoRiftError }; } output[State0]; index 0u32; key "cause";
        flags[true, false]; bound[IntoRiftError]; value value =>
        [ErrorValue::cause(value)]; optional[maybe_cause]; } hit { imports { use super::
        { Builder, Display, ErrorValue, SetState }; } output[SetState]; index 1u32; key
        "hit"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error project_hit_identity_undecodable; imports { use super:: { Display,
        ErrorValue, SetState }; } metadata["rift.mcp.project_hit_identity_undecodable",
        "project hit identity is not a qualified name",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { hit { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "hit"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error project_hit_name_unmatched; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.mcp.project_hit_name_unmatched",
        "project hit name does not match requested name",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { hit { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "hit"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error project_hit_unit_invalid; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.mcp.project_hit_unit_invalid",
        "project hit unit is not a source unit address",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { hit { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "hit"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error proxy_identity_failed; imports { use super:: { Box, Error, ErrorValue,
        SetState }; } metadata["rift.mcp.proxy_identity_failed",
        "MCP proxy could not determine product identity",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { source { imports { use super:: {
        Box, Builder, Error, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error proxy_initialization_failed; imports { use super:: { Box, Error,
        ErrorValue, SetState }; } metadata["rift.mcp.proxy_initialization_failed",
        "MCP proxy initialization failed", "resend the same request after a short delay",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { source {
        imports { use super:: { Box, Builder, Error, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "source"; flags[true, false]; bound[Into < Box
        < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error proxy_task_failed; imports { use super:: { Box, Error, ErrorValue, SetState
        }; } metadata["rift.mcp.proxy_task_failed", "MCP proxy service task failed",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { source { imports { use super:: {
        Box, Builder, Error, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error proxy_unexpected_quit; imports {}
        metadata["rift.mcp.proxy_unexpected_quit", "MCP service ended unexpectedly",
        "report this internal failure with its full context", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error spawn_failed; imports { use super:: { Borrow, Display, ErrorValue, SetState
        }; } metadata["rift.mcp.spawn_failed", "spawned server exited before serving",
        "report this internal failure with its full context", 2usize]; builder Builder;
        states[State0]; complete[SetState]; fields { stderr { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "stderr"; flags[true, true]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } stderr_truncated { imports { use
        super:: { Borrow, Builder, ErrorValue }; } output[State0]; index 1u32; key
        "stderr_truncated"; flags[true, false]; bound[Borrow < bool >]; value value =>
        [ErrorValue::bool_value(* value.borrow())]; optional[maybe_stderr_truncated]; } }
    }
    __rift_error_definition! {
        error spawn_no_output; imports {} metadata["rift.mcp.spawn_no_output",
        "spawned server exited before serving and wrote no standard error",
        "report this internal failure with its full context", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error start_building; imports { use super:: { Borrow, Duration, ErrorValue,
        SetState }; } metadata["rift.mcp.start_building",
        "workspace server did not finish building its first index within {waited}",
        "resend the same request after a short delay", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { waited { imports { use super:: {
        Borrow, Builder, Duration, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "waited"; flags[true, false]; bound[Borrow < Duration >]; value value
        => [ErrorValue::duration(value)]; optional[]; } }
    }
}
#[allow(missing_docs)]
pub mod provider {
    use super::{Display, ErrorValue, SetState, __rift_error_definition};
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error cache_too_many_keys; imports {}
        metadata["rift.provider.cache_too_many_keys",
        "cache key count exceeds its configured limit",
        "reduce cache keys to its configured limit and retry", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error composition_dangling_input; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.provider.composition_dangling_input",
        "composition stage still has a consumer",
        "remove consumers before removing stage and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { stage { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "stage"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error composition_duplicate_stage; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.provider.composition_duplicate_stage",
        "composition stage path already exists", "use a unique stage path and retry",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { stage {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "stage"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error composition_foreign_flow; imports {}
        metadata["rift.provider.composition_foreign_flow",
        "composition flow belongs to another builder",
        "use flow handles from this builder and retry", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error composition_invalid_name; imports {}
        metadata["rift.provider.composition_invalid_name",
        "composition stage name is invalid", "use a canonical stage name and retry",
        0usize]; builder Builder; states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error composition_missing_output; imports {}
        metadata["rift.provider.composition_missing_output",
        "composition has no selected output", "select one output stage and retry",
        0usize]; builder Builder; states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error composition_stage_not_found; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.provider.composition_stage_not_found",
        "composition stage does not exist", "use an existing stage path and retry",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { stage {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "stage"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error composition_type_mismatch; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.provider.composition_type_mismatch",
        "composition stage input and output types do not match",
        "connect stages with matching types and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { stage { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "stage"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error publication_contribution_limit; imports { use super:: { Display,
        ErrorValue, SetState }; }
        metadata["rift.provider.publication_contribution_limit",
        "total contribution count exceeds its configured limit",
        "reduce total contributions to configured limit and retry", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { field { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error publication_duplicate_symbol; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.provider.publication_duplicate_symbol",
        "provider publication repeats a provider symbol",
        "publish each provider symbol once and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { field { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error publication_provider_contribution_limit; imports { use super:: { Display,
        ErrorValue, SetState }; }
        metadata["rift.provider.publication_provider_contribution_limit",
        "provider contribution count exceeds its configured limit",
        "reduce provider contributions to configured limit and retry", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { field { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error publication_provider_limit; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.provider.publication_provider_limit",
        "provider publication count exceeds its configured limit",
        "reduce provider publications to configured limit and retry", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { field { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error publication_provider_mismatch; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.provider.publication_provider_mismatch",
        "contribution provider does not match publication provider",
        "publish contributions under matching provider identity and retry", 1usize];
        builder Builder; states[State0]; complete[SetState]; fields { field { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error publication_revision_mismatch; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.provider.publication_revision_mismatch",
        "contribution revision does not match publication revision",
        "publish contributions under matching revision and retry", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { field { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error publication_zero_limit; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.provider.publication_zero_limit",
        "provider publication limit is zero",
        "set each provider publication limit above zero and retry", 1usize]; builder
        Builder; states[State0]; complete[SetState]; fields { field { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
}
#[allow(missing_docs)]
pub mod ranking {
    use super::{
        Box, Display, Error, ErrorValue, IntoUnsigned, SetState, __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error capabilities_incompatible; imports {}
        metadata["rift.ranking.capabilities_incompatible",
        "index capabilities cannot be combined for ranking",
        "report this internal failure with its full context", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error document_field_length; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.ranking.document_field_length",
        "document field {subject} exceeds its accepted byte limit of {limit} bytes",
        "report this internal failure with its full context", 4usize]; builder Builder;
        states[State0, State1, State2, State3]; complete[SetState, SetState, SetState,
        SetState]; fields { field { imports { use super:: { Builder, Display, ErrorValue,
        SetState }; } output[SetState, State1, State2, State3]; index 0u32; key "field";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[]; } limit { imports { use super:: { Builder, ErrorValue, IntoUnsigned,
        SetState }; } output[State0, SetState, State2, State3]; index 1u32; key "limit";
        flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } required { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, State1, SetState,
        State3]; index 2u32; key "required"; flags[true, false]; bound[IntoUnsigned];
        value value => [ErrorValue::unsigned(value)]; optional[]; } subject { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[State0, State1,
        State2, SetState]; index 3u32; key "subject"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error document_identity_empty; imports { use super:: { Display, ErrorValue }; }
        metadata["rift.ranking.document_identity_empty", "document identity is empty",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[]; complete[]; fields { subject { imports { use super:: { Builder,
        Display, ErrorValue }; } output[]; index 0u32; key "subject"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_subject]; } }
    }
    __rift_error_definition! {
        error fusion_constant_invalid; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.ranking.fusion_constant_invalid",
        "ranking constant is outside its accepted range",
        "set the ranking constant within its accepted range, then retry", 1usize];
        builder Builder; states[State0]; complete[SetState]; fields { subject { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "subject"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error pattern_size; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.ranking.pattern_size",
        "compiled pattern exceeds its accepted size: {subject}",
        "reduce the compiled pattern size and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { subject { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "subject"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error pattern_syntax; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.ranking.pattern_syntax", "pattern syntax is invalid: {subject}",
        "correct the pattern syntax and retry", 1usize]; builder Builder; states[State0];
        complete[SetState]; fields { subject { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState]; index 0u32; key "subject";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[]; } }
    }
    __rift_error_definition! {
        error query_empty; imports { use super:: { Display, ErrorValue }; }
        metadata["rift.ranking.query_empty", "query is empty",
        "provide query text and resend the request", 1usize]; builder Builder; states[];
        complete[]; fields { subject { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 0u32; key "subject"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_subject]; } }
    }
    __rift_error_definition! {
        error query_length; imports { use super:: { Display, ErrorValue, IntoUnsigned,
        SetState }; } metadata["rift.ranking.query_length",
        "query exceeds its accepted byte limit of {limit} bytes",
        "shorten the query below {limit} bytes and resend the request", 4usize]; builder
        Builder; states[State0, State1, State2, State3]; complete[SetState, SetState,
        SetState, SetState]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState, State1, State2, State3]; index 0u32;
        key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } limit { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, SetState, State2,
        State3]; index 1u32; key "limit"; flags[true, false]; bound[IntoUnsigned]; value
        value => [ErrorValue::unsigned(value)]; optional[]; } required { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, State1,
        SetState, State3]; index 2u32; key "required"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        subject { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, State1, State2, SetState]; index 3u32; key "subject"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } }
    }
    __rift_error_definition! {
        error query_phrase_limit; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.ranking.query_phrase_limit",
        "query contains more quoted phrases than the accepted limit of {limit}",
        "reduce quoted phrases to {limit} and resend the request", 4usize]; builder
        Builder; states[State0, State1, State2, State3]; complete[SetState, SetState,
        SetState, SetState]; fields { field { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState, State1, State2, State3]; index 0u32;
        key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } limit { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, SetState, State2,
        State3]; index 1u32; key "limit"; flags[true, false]; bound[IntoUnsigned]; value
        value => [ErrorValue::unsigned(value)]; optional[]; } required { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, State1,
        SetState, State3]; index 2u32; key "required"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        subject { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, State1, State2, SetState]; index 3u32; key "subject"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } }
    }
    __rift_error_definition! {
        error query_quote_unterminated; imports { use super:: { Display, ErrorValue }; }
        metadata["rift.ranking.query_quote_unterminated",
        "query contains an unclosed quote",
        "close every quoted phrase and resend the request", 1usize]; builder Builder;
        states[]; complete[]; fields { subject { imports { use super:: { Builder,
        Display, ErrorValue }; } output[]; index 0u32; key "subject"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_subject]; } }
    }
    __rift_error_definition! {
        error query_term_length; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.ranking.query_term_length",
        "query term exceeds its accepted byte limit of {limit} bytes",
        "shorten the query term below {limit} bytes and resend the request", 4usize];
        builder Builder; states[State0, State1, State2, State3]; complete[SetState,
        SetState, SetState, SetState]; fields { field { imports { use super:: { Builder,
        Display, ErrorValue, SetState }; } output[SetState, State1, State2, State3];
        index 0u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } limit { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, SetState, State2,
        State3]; index 1u32; key "limit"; flags[true, false]; bound[IntoUnsigned]; value
        value => [ErrorValue::unsigned(value)]; optional[]; } required { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, State1,
        SetState, State3]; index 2u32; key "required"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        subject { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, State1, State2, SetState]; index 3u32; key "subject"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } }
    }
    __rift_error_definition! {
        error ranking_weights_invalid; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.ranking.ranking_weights_invalid",
        "ranking shares must be finite values from zero to one with a positive sum",
        "set finite ranking shares from zero to one with a positive sum, then retry",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { subject {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "subject"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error reader_failed; imports { use super:: { Box, Display, Error, ErrorValue,
        SetState }; } metadata["rift.ranking.reader_failed", "search reader failed",
        "check filesystem permissions and free space, then retry", 2usize]; builder
        Builder; states[State0]; complete[SetState]; fields { source { imports { use
        super:: { Box, Builder, Error, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send +
        Sync + 'static >>]; value value => [ErrorValue::source(value)]; optional[]; }
        subject { imports { use super:: { Builder, Display, ErrorValue }; }
        output[State0]; index 1u32; key "subject"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[maybe_subject]; } }
    }
}
#[allow(missing_docs)]
pub mod search {
    use super::{
        Box, Display, Error, ErrorValue, IntoUnsigned, Path, SetState,
        __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error encode_failed; imports { use super:: { Box, Display, Error, ErrorValue }; }
        metadata["rift.search.encode_failed", "text encoding failed",
        "check model input and retry", 2usize]; builder Builder; states[]; complete[];
        fields { source { imports { use super:: { Box, Builder, Error, ErrorValue }; }
        output[]; index 0u32; key "source"; flags[true, false]; bound[Into < Box < dyn
        Error + Send + Sync + 'static >>]; value value => [ErrorValue::source(value)];
        optional[maybe_source]; } stage { imports { use super:: { Builder, Display,
        ErrorValue }; } output[]; index 1u32; key "stage"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)];
        optional[maybe_stage]; } }
    }
    __rift_error_definition! {
        error model_cache_unavailable; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.search.model_cache_unavailable",
        "model cache directory could not be resolved from environment variables {variables}",
        "set a model cache directory and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { variables { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "variables"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error model_configuration_invalid; imports { use super:: { Box, Error,
        ErrorValue, Path }; } metadata["rift.search.model_configuration_invalid",
        "model configuration is invalid",
        "provide a model configuration this encoder serves and retry", 2usize]; builder
        Builder; states[]; complete[]; fields { path { imports { use super:: { Builder,
        ErrorValue, Path }; } output[]; index 0u32; key "path"; flags[true, false];
        bound[AsRef < Path >]; value value => [ErrorValue::path(value)];
        optional[maybe_path]; } source { imports { use super:: { Box, Builder, Error,
        ErrorValue }; } output[]; index 1u32; key "source"; flags[true, false];
        bound[Into < Box < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error model_download_failed; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.search.model_download_failed",
        "model file download failed: {subject}", "check network access and retry",
        2usize]; builder Builder; states[State0]; complete[SetState]; fields { source {
        imports { use super:: { Box, Builder, Error, ErrorValue }; } output[State0];
        index 0u32; key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send
        + Sync + 'static >>]; value value => [ErrorValue::source(value)];
        optional[maybe_source]; } subject { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState]; index 1u32; key "subject";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[]; } }
    }
    __rift_error_definition! {
        error model_download_too_large; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.search.model_download_too_large",
        "model file body at {url} is empty or exceeds accepted byte limit {bytes_max}",
        "provide a nonempty model file within its byte limit and retry", 2usize]; builder
        Builder; states[State0, State1]; complete[SetState, SetState]; fields { bytes_max
        { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[SetState, State1]; index 0u32; key "bytes_max"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        url { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "url"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error model_file_missing; imports { use super:: { ErrorValue, Path, SetState }; }
        metadata["rift.search.model_file_missing",
        "model directory is missing file {subject}",
        "supply the missing model file and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { subject { imports { use super:: {
        Builder, ErrorValue, Path, SetState }; } output[SetState]; index 0u32; key
        "subject"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error model_source_invalid; imports { use super:: { Box, Display, Error,
        ErrorValue, SetState }; } metadata["rift.search.model_source_invalid",
        "model source {model} has invalid form; expected {expected}",
        "use a model source in the expected form and retry", 3usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { expected { imports
        { use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "expected"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } model { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "model"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } source { imports { use super:: { Box,
        Builder, Error, ErrorValue }; } output[State0, State1]; index 2u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error task_failed; imports { use super:: { Box, Error, ErrorValue }; }
        metadata["rift.search.task_failed", "search task did not return", "retry search",
        1usize]; builder Builder; states[]; complete[]; fields { source { imports { use
        super:: { Box, Builder, Error, ErrorValue }; } output[]; index 0u32; key
        "source"; flags[true, false]; bound[Into < Box < dyn Error + Send + Sync +
        'static >>]; value value => [ErrorValue::source(value)]; optional[maybe_source];
        } }
    }
    __rift_error_definition! {
        error text_limit; imports { use super:: { ErrorValue, IntoUnsigned, SetState }; }
        metadata["rift.search.text_limit",
        "encoder received {observed} texts, exceeding accepted limit {limit}",
        "reduce input texts below {limit} and retry", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { limit { imports {
        use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState,
        State1]; index 0u32; key "limit"; flags[true, false]; bound[IntoUnsigned]; value
        value => [ErrorValue::unsigned(value)]; optional[]; } observed { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0,
        SetState]; index 1u32; key "observed"; flags[true, false]; bound[IntoUnsigned];
        value value => [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error tokenizer_unreadable; imports { use super:: { Box, Error, ErrorValue, Path
        }; } metadata["rift.search.tokenizer_unreadable",
        "model tokenizer is unreadable", "repair the model tokenizer file and retry",
        2usize]; builder Builder; states[]; complete[]; fields { path { imports { use
        super:: { Builder, ErrorValue, Path }; } output[]; index 0u32; key "path";
        flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } source { imports { use super::
        { Box, Builder, Error, ErrorValue }; } output[]; index 1u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[maybe_source]; } }
    }
    __rift_error_definition! {
        error vector_coordinate_invalid; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.search.vector_coordinate_invalid",
        "embedded coordinate {coordinate} cannot be represented in the stored f32 format",
        "use an embedding model that returns finite coordinates within the stored range",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { coordinate
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "coordinate"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error vector_width_mismatch; imports { use super:: { ErrorValue, IntoUnsigned,
        SetState }; } metadata["rift.search.vector_width_mismatch",
        "query vector width {query_width} does not match stored vector width {stored_width}",
        "use vectors with the stored width and retry", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { query_width {
        imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[SetState, State1]; index 0u32; key "query_width"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        stored_width { imports { use super:: { Builder, ErrorValue, IntoUnsigned,
        SetState }; } output[State0, SetState]; index 1u32; key "stored_width";
        flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error weights_unreadable; imports { use super:: { Box, Error, ErrorValue, Path };
        } metadata["rift.search.weights_unreadable", "model weights are unreadable",
        "repair the model weights file and retry", 2usize]; builder Builder; states[];
        complete[]; fields { path { imports { use super:: { Builder, ErrorValue, Path };
        } output[]; index 0u32; key "path"; flags[true, false]; bound[AsRef < Path >];
        value value => [ErrorValue::path(value)]; optional[maybe_path]; } source {
        imports { use super:: { Box, Builder, Error, ErrorValue }; } output[]; index
        1u32; key "source"; flags[true, false]; bound[Into < Box < dyn Error + Send +
        Sync + 'static >>]; value value => [ErrorValue::source(value)];
        optional[maybe_source]; } }
    }
}
#[allow(missing_docs)]
pub mod server {
    use super::{
        Display, ErrorValue, IntoRiftError, IntoUnsigned, SetState,
        __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error read_cancelled; imports {} metadata["rift.server.read_cancelled",
        "workspace read was cancelled",
        "resend the request if the result is still needed", 0usize]; builder Builder;
        states[]; complete[]; fields {}
    }
    __rift_error_definition! {
        error read_capacity_timeout; imports { use super:: { Display, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.server.read_capacity_timeout",
        "workspace read waited too long for blocking capacity during {operation}",
        "resend the same request after a short delay", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { operation {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1]; index 0u32; key "operation"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; }
        timeout_ms { imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState
        }; } output[State0, SetState]; index 1u32; key "timeout_ms"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        }
    }
    __rift_error_definition! {
        error read_engine_answer; imports { use super:: { Display, ErrorValue, SetState
        }; } metadata["rift.server.read_engine_answer",
        "the addressed content exists but its bytes cannot be served: operation {operation}, detail {detail}",
        "read the request again once the engine has read the served revision", 2usize];
        builder Builder; states[State0, State1]; complete[SetState, SetState]; fields {
        detail { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1]; index 0u32; key "detail"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; }
        operation { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, SetState]; index 1u32; key "operation"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error read_invalid; imports { use super:: { Display, ErrorValue, IntoRiftError,
        SetState }; } metadata["rift.server.read_invalid",
        "the request does not match the documented form: field {field}, violation {violation}",
        "correct the reported field and resend the request", 3usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { cause { imports {
        use super:: { Builder, ErrorValue, IntoRiftError }; } output[State0, State1];
        index 0u32; key "cause"; flags[true, false]; bound[IntoRiftError]; value value =>
        [ErrorValue::cause(value)]; optional[maybe_cause]; } field { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[SetState, State1];
        index 1u32; key "field"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } violation { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[State0, SetState]; index 2u32;
        key "violation"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error read_not_found; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.server.read_not_found",
        "requested source path does not exist: {path}",
        "search or list first, then retry with a path that answer returned", 1usize];
        builder Builder; states[State0]; complete[SetState]; fields { path { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState]; index
        0u32; key "path"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error read_source_unavailable; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.server.read_source_unavailable",
        "claimed source path is not available in the index: {path}",
        "request the declaration without its body, or read a source-backed unit",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { path {
        imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "path"; flags[true, false]; bound[Display];
        value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error read_storage; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.server.read_storage",
        "workspace file operation failed: {operation}",
        "check filesystem permissions and free space, then retry", 3usize]; builder
        Builder; states[State0, State1, State2]; complete[SetState, SetState, SetState];
        fields { io { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "io"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; }
        operation { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, SetState, State2]; index 1u32; key "operation"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } path { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[State0, State1, SetState]; index 2u32; key "path"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error read_task; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.server.read_task", "workspace read task failed: {operation}",
        "report this internal failure with its full context", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { detail { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "detail"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } operation { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "operation"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error read_unavailable; imports { use super:: { Display, ErrorValue, SetState };
        } metadata["rift.server.read_unavailable",
        "workspace index is unavailable during {operation}",
        "resend the same request after a short delay", 2usize]; builder Builder;
        states[State0, State1]; complete[SetState, SetState]; fields { detail { imports {
        use super:: { Builder, Display, ErrorValue, SetState }; } output[SetState,
        State1]; index 0u32; key "detail"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } operation { imports { use
        super:: { Builder, Display, ErrorValue, SetState }; } output[State0, SetState];
        index 1u32; key "operation"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error read_unclaimed_extension; imports { use super:: { Display, ErrorValue,
        SetState }; } metadata["rift.server.read_unclaimed_extension",
        "no shipped syntax provider parses path extension {extension}",
        "address a path a shipped provider parses", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { extension { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "extension"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error read_unsupported; imports { use super:: { Display, ErrorValue, SetState };
        } metadata["rift.server.read_unsupported",
        "request uses unsupported capability {capability}",
        "adjust the request to a served capability, or configure a provider that serves it",
        1usize]; builder Builder; states[State0]; complete[SetState]; fields { capability
        { imports { use super:: { Builder, Display, ErrorValue, SetState }; }
        output[SetState]; index 0u32; key "capability"; flags[true, false];
        bound[Display]; value value => [ErrorValue::display(value)]; optional[]; } }
    }
}
#[allow(missing_docs)]
pub mod syntax {
    use super::{
        Box, Display, Error, ErrorValue, IntoUnsigned, Path, SetState,
        __rift_error_definition,
    };
    pub use super::{FieldSet, OptionalFieldSet};
    __rift_error_definition! {
        error incompatible_grammar; imports { use super:: { ErrorValue, IntoUnsigned,
        SetState }; } metadata["rift.syntax.incompatible_grammar",
        "grammar ABI {grammar_abi_version} is outside runtime range {runtime_abi_min} to {runtime_abi_max}",
        "use a grammar built for the accepted runtime ABI range", 3usize]; builder
        Builder; states[State0, State1, State2]; complete[SetState, SetState, SetState];
        fields { grammar_abi_version { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned, SetState }; } output[SetState, State1, State2]; index 0u32; key
        "grammar_abi_version"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } runtime_abi_max { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0,
        SetState, State2]; index 1u32; key "runtime_abi_max"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        runtime_abi_min { imports { use super:: { Builder, ErrorValue, IntoUnsigned,
        SetState }; } output[State0, State1, SetState]; index 2u32; key
        "runtime_abi_min"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error invalid_markdown_ranges; imports { use super:: { ErrorValue, Path, SetState
        }; } metadata["rift.syntax.invalid_markdown_ranges",
        "Tree-sitter rejected Markdown inline ranges",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { path { imports { use super:: {
        Builder, ErrorValue, Path, SetState }; } output[SetState]; index 0u32; key
        "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error invalid_query; imports { use super:: { Box, Display, Error, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.syntax.invalid_query",
        "syntax query is invalid at line {line_number}: {line_text}",
        "correct the query line and retry", 3usize]; builder Builder; states[State0,
        State1, State2]; complete[SetState, SetState, SetState]; fields { line_number {
        imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[SetState, State1, State2]; index 0u32; key "line_number"; flags[true,
        false]; bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)];
        optional[]; } line_text { imports { use super:: { Builder, Display, ErrorValue,
        SetState }; } output[State0, SetState, State2]; index 1u32; key "line_text";
        flags[true, false]; bound[Display]; value value => [ErrorValue::display(value)];
        optional[]; } source { imports { use super:: { Box, Builder, Error, ErrorValue,
        SetState }; } output[State0, State1, SetState]; index 2u32; key "source";
        flags[true, false]; bound[Into < Box < dyn Error + Send + Sync + 'static >>];
        value value => [ErrorValue::source(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error markdown_progress_exceeded; imports { use super:: { ErrorValue,
        IntoUnsigned, Path, SetState }; }
        metadata["rift.syntax.markdown_progress_exceeded",
        "Markdown parser exceeded progress callback limit {progress_callbacks_max}",
        "reduce Markdown source size and retry", 2usize]; builder Builder; states[State0,
        State1]; complete[SetState, SetState]; fields { path { imports { use super:: {
        Builder, ErrorValue, Path, SetState }; } output[SetState, State1]; index 0u32;
        key "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[]; } progress_callbacks_max { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0,
        SetState]; index 1u32; key "progress_callbacks_max"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        }
    }
    __rift_error_definition! {
        error parse_cancelled; imports { use super:: { ErrorValue, Path }; }
        metadata["rift.syntax.parse_cancelled", "syntax parser returned no tree",
        "retry syntax parsing", 1usize]; builder Builder; states[]; complete[]; fields {
        path { imports { use super:: { Builder, ErrorValue, Path }; } output[]; index
        0u32; key "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } }
    }
    __rift_error_definition! {
        error position_overflow; imports { use super:: { Box, Display, Error, ErrorValue,
        IntoUnsigned, SetState }; } metadata["rift.syntax.position_overflow",
        "syntax node position does not fit the accepted byte width",
        "report this internal failure with its full context", 4usize]; builder Builder;
        states[State0, State1, State2, State3]; complete[SetState, SetState, SetState,
        SetState]; fields { end_byte { imports { use super:: { Builder, ErrorValue,
        IntoUnsigned, SetState }; } output[SetState, State1, State2, State3]; index 0u32;
        key "end_byte"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } node_kind { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[State0, SetState, State2,
        State3]; index 1u32; key "node_kind"; flags[true, false]; bound[Display]; value
        value => [ErrorValue::display(value)]; optional[]; } source { imports { use
        super:: { Box, Builder, Error, ErrorValue, SetState }; } output[State0, State1,
        SetState, State3]; index 2u32; key "source"; flags[true, false]; bound[Into < Box
        < dyn Error + Send + Sync + 'static >>]; value value =>
        [ErrorValue::source(value)]; optional[]; } start_byte { imports { use super:: {
        Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0, State1, State2,
        SetState]; index 3u32; key "start_byte"; flags[true, false]; bound[IntoUnsigned];
        value value => [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error source_too_large; imports { use super:: { ErrorValue, IntoUnsigned, Path,
        SetState }; } metadata["rift.syntax.source_too_large",
        "source bytes {source_bytes} exceed accepted limit {source_bytes_max}",
        "reduce source bytes below {source_bytes_max} and retry", 3usize]; builder
        Builder; states[State0, State1]; complete[SetState, SetState]; fields { path {
        imports { use super:: { Builder, ErrorValue, Path }; } output[State0, State1];
        index 0u32; key "path"; flags[true, false]; bound[AsRef < Path >]; value value =>
        [ErrorValue::path(value)]; optional[maybe_path]; } source_bytes { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState,
        State1]; index 1u32; key "source_bytes"; flags[true, false]; bound[IntoUnsigned];
        value value => [ErrorValue::unsigned(value)]; optional[]; } source_bytes_max {
        imports { use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; }
        output[State0, SetState]; index 2u32; key "source_bytes_max"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        }
    }
    __rift_error_definition! {
        error too_deep; imports { use super:: { ErrorValue, IntoUnsigned, Path, SetState
        }; } metadata["rift.syntax.too_deep",
        "syntax tree depth exceeds accepted limit {syntax_depth_max}",
        "reduce syntax tree depth below {syntax_depth_max} and retry", 2usize]; builder
        Builder; states[State0, State1]; complete[SetState, SetState]; fields { path {
        imports { use super:: { Builder, ErrorValue, Path, SetState }; } output[SetState,
        State1]; index 0u32; key "path"; flags[true, false]; bound[AsRef < Path >]; value
        value => [ErrorValue::path(value)]; optional[]; } syntax_depth_max { imports {
        use super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0,
        SetState]; index 1u32; key "syntax_depth_max"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        }
    }
    __rift_error_definition! {
        error too_many_captures; imports { use super:: { ErrorValue, IntoUnsigned,
        SetState }; } metadata["rift.syntax.too_many_captures",
        "syntax query produced more than {captures_max} captures",
        "reduce query captures below {captures_max} and retry", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { captures_max { imports { use super::
        { Builder, ErrorValue, IntoUnsigned, SetState }; } output[SetState]; index 0u32;
        key "captures_max"; flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error too_many_markdown_inline_ranges; imports { use super:: { ErrorValue,
        IntoUnsigned, Path, SetState }; }
        metadata["rift.syntax.too_many_markdown_inline_ranges",
        "Markdown inline ranges {observed} exceed accepted limit {inline_ranges_max}",
        "reduce Markdown inline ranges below {inline_ranges_max} and retry", 3usize];
        builder Builder; states[State0, State1, State2]; complete[SetState, SetState,
        SetState]; fields { inline_ranges_max { imports { use super:: { Builder,
        ErrorValue, IntoUnsigned, SetState }; } output[SetState, State1, State2]; index
        0u32; key "inline_ranges_max"; flags[true, false]; bound[IntoUnsigned]; value
        value => [ErrorValue::unsigned(value)]; optional[]; } observed { imports { use
        super:: { Builder, ErrorValue, IntoUnsigned, SetState }; } output[State0,
        SetState, State2]; index 1u32; key "observed"; flags[true, false];
        bound[IntoUnsigned]; value value => [ErrorValue::unsigned(value)]; optional[]; }
        path { imports { use super:: { Builder, ErrorValue, Path, SetState }; }
        output[State0, State1, SetState]; index 2u32; key "path"; flags[true, false];
        bound[AsRef < Path >]; value value => [ErrorValue::path(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error too_many_nodes; imports { use super:: { ErrorValue, IntoUnsigned, Path,
        SetState }; } metadata["rift.syntax.too_many_nodes",
        "syntax tree node count exceeds accepted limit {syntax_nodes_max}",
        "reduce syntax tree node count below {syntax_nodes_max} and retry", 2usize];
        builder Builder; states[State0, State1]; complete[SetState, SetState]; fields {
        path { imports { use super:: { Builder, ErrorValue, Path, SetState }; }
        output[SetState, State1]; index 0u32; key "path"; flags[true, false]; bound[AsRef
        < Path >]; value value => [ErrorValue::path(value)]; optional[]; }
        syntax_nodes_max { imports { use super:: { Builder, ErrorValue, IntoUnsigned,
        SetState }; } output[State0, SetState]; index 1u32; key "syntax_nodes_max";
        flags[true, false]; bound[IntoUnsigned]; value value =>
        [ErrorValue::unsigned(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error unknown_node_kind; imports { use super:: { Display, ErrorValue, SetState };
        } metadata["rift.syntax.unknown_node_kind",
        "node kind {node_kind} is outside interpreted grammar vocabulary",
        "report this internal failure with its full context", 1usize]; builder Builder;
        states[State0]; complete[SetState]; fields { node_kind { imports { use super:: {
        Builder, Display, ErrorValue, SetState }; } output[SetState]; index 0u32; key
        "node_kind"; flags[true, false]; bound[Display]; value value =>
        [ErrorValue::display(value)]; optional[]; } }
    }
    __rift_error_definition! {
        error zero_limit; imports { use super:: { Display, ErrorValue, SetState }; }
        metadata["rift.syntax.zero_limit", "syntax bound {bound} is zero",
        "set syntax bounds above zero", 1usize]; builder Builder; states[State0];
        complete[SetState]; fields { bound { imports { use super:: { Builder, Display,
        ErrorValue, SetState }; } output[SetState]; index 0u32; key "bound"; flags[true,
        false]; bound[Display]; value value => [ErrorValue::display(value)]; optional[];
        } }
    }
}
