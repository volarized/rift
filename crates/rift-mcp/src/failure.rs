//! Wire projection of operating failures.
//!
//! A tool path returns an operating failure as a JSON-RPC error object, and `RiftMcp::call_tool`
//! converts it into a completed tool result with `isError` and compact text. The JSON-RPC form
//! remains the reply to a failure raised outside a tool call, such as the proxy or the transport
//! guard.

use rift_error::{RiftError, errors};
use rift_protocol::error as wire;
use rmcp::ErrorData;
use rmcp::model::ErrorCode;
use std::fmt;

use crate::output::ToolFailure;

/// JSON-RPC error code of the error object that carries a Rift operating failure: the
/// first code of the server-defined range (-32000 to -32099), which rmcp
/// exports no constant for - its constants name only MCP-defined codes. The
/// machine-readable classification is the [`wire::ErrorData`] in `data`.
/// `RiftMcp::call_tool` reads this code to complete a tool-path failure as a result with
/// `isError`; a failure raised outside a tool call is served under this code as the error
/// object.
pub(crate) const RIFT_ERROR_CODE: ErrorCode = ErrorCode(-32000);

/// Most `causes` entries one wire error carries, matching the advertised
/// schema bound.
pub(crate) const ERROR_CAUSES_MAX: usize = 8;

/// Boundary view of a read failure: the JSON-RPC error object a tool path
/// returns - code `-32000`, the rendered failure line as `message`, and the
/// typed [`wire::ErrorData`] as `data`. `RiftMcp::call_tool` completes that
/// object as a tool result with `isError`; a failure raised outside a tool
/// call is served as the object itself.
pub(crate) trait WireFailure {
    /// The JSON-RPC error object for this failure, naming the phase it
    /// stopped in.
    fn tool_error(&self, phase: wire::ErrorPhase) -> ErrorData;

    /// The typed wire payload for this failure.
    fn wire_error(&self, phase: wire::ErrorPhase) -> wire::ErrorData;

    /// The failure's source chain as bounded `causes` entries, outermost
    /// first. Each level inherits the outer classification, which the read
    /// error already resolved through the concrete failure it wraps.
    fn wire_causes(&self) -> Vec<wire::ErrorCause>;
}

/// Rift error prepared for the MCP wire boundary.
#[derive(Debug)]
pub struct McpFailure {
    error: RiftError,
}

impl fmt::Display for McpFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for McpFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(&self.error)
    }
}

impl McpFailure {
    /// Converts one registered error for MCP wire rendering.
    #[must_use]
    pub fn new(error: RiftError) -> Self {
        Self { error }
    }

    /// Returns registered identity for CLI boundary code selection.
    #[must_use]
    pub fn slug(&self) -> rift_error::ErrorSlug {
        self.error.slug()
    }

    /// Returns wire code for this failure.
    #[must_use]
    pub fn wire_code(&self) -> String {
        wire_code_for_error(&self.error).unwrap_or_else(|| self.error.slug().as_str().to_owned())
    }

    /// The registered error this failure carries.
    pub(crate) const fn registered(&self) -> &RiftError {
        &self.error
    }

    /// The failure of a tool call that stopped in `phase`, carrying this registered error to
    /// the text of the completed result.
    pub(crate) const fn tool_failure(self, phase: wire::ErrorPhase) -> ToolFailure {
        ToolFailure::Registered(self, phase)
    }
}

rift_error::format! {
    mcp -> McpFailure as McpErrorExt, McpErrorFailExt using McpFailure::new
}

impl McpErrorFailExt for ErrorData {}

impl McpErrorFailExt for ToolFailure {}

impl WireFailure for McpFailure {
    fn tool_error(&self, phase: wire::ErrorPhase) -> ErrorData {
        let data = serde_json::to_value(self.wire_error(phase)).ok();
        ErrorData::new(RIFT_ERROR_CODE, self.error.to_string(), data)
    }

    fn wire_error(&self, phase: wire::ErrorPhase) -> wire::ErrorData {
        let (code, retry) = wire_guidance_for_error(&self.error).unwrap_or((
            wire::ErrorCode::InternalError,
            wire::RetryDirective::SameRequest,
        ));
        wire::ErrorData {
            code,
            message: self.error.to_string(),
            retry,
            phase,
            diagnostics: Vec::new(),
            limit: wire_limit(&self.error, code),
            causes: self.wire_causes(),
        }
    }

    fn wire_causes(&self) -> Vec<wire::ErrorCause> {
        let (code, retry) = wire_guidance_for_error(&self.error).unwrap_or((
            wire::ErrorCode::InternalError,
            wire::RetryDirective::SameRequest,
        ));
        bounded_causes(code, retry, std::error::Error::source(&self.error))
    }
}

/// Walks one source chain into bounded `causes` entries, outermost first.
/// Every level inherits the classification and retry guidance passed in,
/// which the failure already resolved through the concrete fault it wraps.
pub(crate) fn bounded_causes(
    code: wire::ErrorCode,
    retry: wire::RetryDirective,
    outermost: Option<&(dyn std::error::Error + 'static)>,
) -> Vec<wire::ErrorCause> {
    let mut causes = Vec::new();
    let mut source = outermost;
    while let Some(current) = source {
        if causes.len() == ERROR_CAUSES_MAX {
            break;
        }
        causes.push(wire::ErrorCause {
            code,
            message: current.to_string(),
            retry,
        });
        source = current.source();
    }
    causes
}

/// The wire code string for one registered identity, for CLI output that
/// passes through MCP.
pub fn wire_code_for_slug(slug: &str) -> Option<String> {
    let (code, _) = wire_guidance(slug)?;
    serde_json::to_value(code).ok()?.as_str().map(str::to_owned)
}

/// Returns wire code for a registered error, including delegated workspace identity.
#[must_use]
pub fn wire_code_for_error(error: &RiftError) -> Option<String> {
    let (code, _) = wire_guidance_for_error(error)?;
    serde_json::to_value(code).ok()?.as_str().map(str::to_owned)
}

fn wire_guidance_for_error(error: &RiftError) -> Option<(wire::ErrorCode, wire::RetryDirective)> {
    let slug = error.slug();
    if slug == errors::index::workspace_syntax::SLUG
        || slug == errors::index::workspace_history::SLUG
    {
        let mut source = std::error::Error::source(error);
        for _ in 0..rift_error::CAUSE_DEPTH_MAX {
            let Some(current) = source else {
                break;
            };
            if let Some(child) = current.downcast_ref::<RiftError>() {
                if let Some(guidance) = wire_guidance(child.slug().as_str()) {
                    return Some(guidance);
                }
                break;
            }
            source = std::error::Error::source(current);
        }
    }
    wire_guidance(slug.as_str())
}

// Exact identities take precedence over namespace defaults. Lookup visits only
// these fixed groups and their fixed slices of identities.
const WIRE_GUIDANCE: &[(wire::ErrorCode, wire::RetryDirective, &[&str])] = {
    use wire::{ErrorCode as Code, RetryDirective as Retry};
    &[
        (
            Code::Cancelled,
            Retry::SameRequest,
            &[
                "rift.index.workspace_cancelled",
                "rift.server.read_cancelled",
                "rift.syntax.parse_cancelled",
            ],
        ),
        (
            Code::CapabilityUnavailable,
            Retry::OperatorAction,
            &[
                "rift.history.unversioned",
                "rift.server.read_unclaimed_extension",
                "rift.server.read_unsupported",
                "rift.tracing.log_stream_unavailable",
            ],
        ),
        (
            Code::ConfigurationInvalid,
            Retry::OperatorAction,
            &[
                "rift.analysis.package_retained_source_limits_invalid",
                "rift.analysis.source_pattern_invalid",
                "rift.index.workspace_composition",
                "rift.index.workspace_invalid_root",
                "rift.index.workspace_language_include_required",
                "rift.index.workspace_language_match_conflict",
                "rift.index.workspace_source_pattern_invalid",
                "rift.index.workspace_zero_limit",
                "rift.lsp.engine_program_absolute",
                "rift.lsp.engine_program_empty",
                "rift.provider.composition_dangling_input",
                "rift.provider.composition_duplicate_stage",
                "rift.provider.composition_foreign_flow",
                "rift.provider.composition_invalid_name",
                "rift.provider.composition_missing_output",
                "rift.provider.composition_stage_not_found",
                "rift.provider.composition_type_mismatch",
                "rift.ranking.fusion_constant_invalid",
                "rift.ranking.ranking_weights_invalid",
                "rift.search.model_cache_unavailable",
                "rift.search.model_configuration_invalid",
                "rift.search.model_source_invalid",
                "rift.search.tokenizer_unreadable",
                "rift.search.vector_coordinate_invalid",
                "rift.search.vector_width_mismatch",
                "rift.search.weights_unreadable",
                "rift.syntax.zero_limit",
                "rift.tracing.log_queue_limit",
            ],
        ),
        (
            Code::ContentUnavailable,
            Retry::Never,
            &[
                "rift.analysis.context7_entry_invalid",
                "rift.analysis.context7_malformed",
                "rift.index.workspace_invalid_source",
                "rift.server.read_engine_answer",
                "rift.server.read_source_unavailable",
            ],
        ),
        (
            Code::InternalError,
            Retry::OperatorAction,
            &[
                "rift.cli.update_binary_invalid",
                "rift.cli.update_publish_copy_failed",
                "rift.cli.update_publish_failed",
                "rift.cli.update_publish_parent_missing",
                "rift.cli.update_publish_pending_cleanup",
                "rift.cli.update_rollback_cleanup_failed",
                "rift.cli.update_rollback_failed",
                "rift.cli.update_staging_failed",
            ],
        ),
        (
            Code::InternalError,
            Retry::SameRequest,
            &[
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
                "rift.cli.update_checksum_manifest_invalid",
                "rift.cli.update_checksum_mismatch",
                "rift.cli.update_checksum_read_failed",
                "rift.cli.update_download_failed",
                "rift.cli.update_download_too_large",
                "rift.cli.update_prerelease_unsupported",
                "rift.cli.update_release_file_inspection_failed",
                "rift.cli.update_release_file_not_regular",
                "rift.cli.update_release_file_size_invalid",
                "rift.cli.update_release_metadata_invalid",
                "rift.cli.update_release_tag_invalid",
                "rift.cli.update_version_invalid",
                "rift.history.contribution_invalid",
                "rift.index.lexical_document_location_unsupported",
                "rift.index.lexical_duplicate_identity",
                "rift.index.lexical_stored_kind_invalid",
                "rift.index.lexical_stored_path_invalid",
                "rift.index.workspace_history",
                "rift.index.workspace_provider",
                "rift.index.workspace_syntax",
                "rift.lsp.position_character_misaligned",
                "rift.lsp.position_character_out_of_range",
                "rift.lsp.position_line_out_of_range",
                "rift.lsp.position_offset_inside_line_ending",
                "rift.lsp.position_offset_misaligned",
                "rift.lsp.position_offset_out_of_range",
                "rift.mcp.answer_structure_failed",
                "rift.mcp.answer_text_failed",
                "rift.mcp.election_already_serving",
                "rift.mcp.election_document_invalid",
                "rift.mcp.http_serve_failed",
                "rift.mcp.project_hit_identity_missing",
                "rift.mcp.project_hit_identity_refused",
                "rift.mcp.project_hit_identity_undecodable",
                "rift.mcp.project_hit_name_unmatched",
                "rift.mcp.project_hit_unit_invalid",
                "rift.mcp.proxy_identity_failed",
                "rift.mcp.proxy_task_failed",
                "rift.mcp.proxy_unexpected_quit",
                "rift.ranking.capabilities_incompatible",
                "rift.ranking.document_field_length",
                "rift.ranking.document_identity_empty",
                "rift.search.encode_failed",
                "rift.search.store_failed",
                "rift.search.task_failed",
                "rift.server.read_task",
            ],
        ),
        (
            Code::InvalidRequest,
            Retry::Never,
            &[
                "rift.analysis.documentation_digest_mismatch",
                "rift.analysis.documentation_duplicate_source",
                "rift.analysis.documentation_encoding_failed",
                "rift.analysis.documentation_format_invalid",
                "rift.analysis.documentation_identity_invalid",
                "rift.analysis.documentation_notebook_invalid",
                "rift.analysis.documentation_order_invalid",
                "rift.analysis.documentation_origin_invalid",
                "rift.analysis.documentation_range_invalid",
                "rift.analysis.documentation_revision_invalid",
                "rift.analysis.package_identity_invalid",
                "rift.analysis.package_input_duplicate_path",
                "rift.analysis.package_input_identity_invalid",
                "rift.analysis.package_input_origin_invalid",
                "rift.analysis.package_provider_failed",
                "rift.analysis.package_syntax_unavailable",
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
                "rift.lsp.engine_refused_terminal",
                "rift.mcp.arguments_not_object",
                "rift.mcp.parameter_invalid",
                "rift.provider.publication_contribution_limit",
                "rift.provider.publication_duplicate_symbol",
                "rift.provider.publication_provider_contribution_limit",
                "rift.provider.publication_provider_limit",
                "rift.provider.publication_provider_mismatch",
                "rift.provider.publication_revision_mismatch",
                "rift.provider.publication_zero_limit",
                "rift.ranking.pattern_size",
                "rift.ranking.pattern_syntax",
                "rift.ranking.query_empty",
                "rift.ranking.query_length",
                "rift.ranking.query_phrase_limit",
                "rift.ranking.query_quote_unterminated",
                "rift.ranking.query_term_length",
                "rift.server.read_invalid",
            ],
        ),
        (
            Code::LimitExceeded,
            Retry::Never,
            &[
                "rift.analysis.context7_oversized",
                "rift.analysis.context7_too_many_entries",
                "rift.analysis.documentation_limit_exceeded",
                "rift.analysis.package_declarations_exceeded",
                "rift.analysis.package_input_too_many_bytes",
                "rift.analysis.package_input_too_many_files",
                "rift.analysis.package_retained_source_bytes_exceeded",
                "rift.history.blob_too_large",
                "rift.history.too_many_tags",
                "rift.history.tree_too_large",
                "rift.index.lexical_record_limit",
                "rift.index.lexical_unit_limit",
                "rift.index.lexical_unit_too_large",
                "rift.index.workspace_documentation_limit",
                "rift.index.workspace_file_too_large",
                "rift.index.workspace_result_limit",
                "rift.index.workspace_too_deep",
                "rift.index.workspace_too_many_files",
                "rift.index.workspace_workspace_too_large",
                "rift.lsp.correlation_pending_requests_exceeded",
                "rift.lsp.framing_header_too_long",
                "rift.lsp.framing_message_too_long",
                "rift.mcp.answer_text_limit",
                "rift.provider.cache_too_many_keys",
                "rift.search.model_download_too_large",
                "rift.search.text_limit",
                "rift.syntax.markdown_progress_exceeded",
                "rift.syntax.source_too_large",
                "rift.syntax.too_deep",
                "rift.syntax.too_many_captures",
                "rift.syntax.too_many_markdown_inline_ranges",
                "rift.syntax.too_many_nodes",
                "rift.tracing.log_batch_limit",
            ],
        ),
        (
            Code::LimitExceeded,
            Retry::OperatorAction,
            &["rift.tracing.log_subscription_limit"],
        ),
        (
            Code::PermissionDenied,
            Retry::Never,
            &["rift.lsp.uri_outside_root"],
        ),
        (
            Code::ResourceNotFound,
            Retry::Never,
            &["rift.server.read_not_found"],
        ),
        (
            Code::ResourceNotFound,
            Retry::OperatorAction,
            &[
                "rift.analysis.documentation_target_missing",
                "rift.history.revision_not_commit",
                "rift.history.revision_unknown",
                "rift.search.model_file_missing",
            ],
        ),
        (
            Code::StorageFailure,
            Retry::SameRequest,
            &[
                "rift.core.configuration_unreadable",
                "rift.history.storage",
                "rift.history_store.database",
                "rift.history_store.folder",
                "rift.index.database_failed",
                "rift.index.lexical_storage",
                "rift.index.workspace_filesystem",
                "rift.mcp.election_storage_failed",
                "rift.ranking.reader_failed",
                "rift.server.read_storage",
                "rift.tracing.log_store_failed",
            ],
        ),
        (
            Code::TemporarilyUnavailable,
            Retry::SameRequest,
            &[
                "rift.history_store.lock_unstable",
                "rift.index.workspace_changed_during_capture",
                "rift.lsp.engine_analyzing",
                "rift.lsp.engine_connection_closed",
                "rift.lsp.engine_ended",
                "rift.lsp.engine_refused_retryable",
                "rift.lsp.engine_timed_out",
                "rift.mcp.forward_unanswered",
                "rift.mcp.http_ports_exhausted",
                "rift.mcp.proxy_initialization_failed",
                "rift.mcp.spawn_failed",
                "rift.mcp.spawn_no_output",
                "rift.mcp.start_building",
                "rift.search.model_download_failed",
                "rift.server.read_capacity_timeout",
                "rift.server.read_unavailable",
            ],
        ),
        (
            Code::UnsupportedPath,
            Retry::Never,
            &[
                "rift.history.path_unrepresentable",
                "rift.index.workspace_invalid_path",
            ],
        ),
    ]
};

fn wire_guidance(slug: &str) -> Option<(wire::ErrorCode, wire::RetryDirective)> {
    use wire::{ErrorCode as Code, RetryDirective as Retry};
    if let Some((code, retry, _)) = WIRE_GUIDANCE
        .iter()
        .find(|(_, _, slugs)| slugs.contains(&slug))
    {
        return Some((*code, *retry));
    }
    let guidance = match slug {
        slug if slug.starts_with("rift.lsp.uri_") || slug.starts_with("rift.core.path_") => {
            (Code::UnsupportedPath, Retry::Never)
        }
        slug if slug.starts_with("rift.lsp.capabilities_")
            || slug == "rift.lsp.correlation_response_unknown"
            || slug.starts_with("rift.lsp.framing_")
            || slug.starts_with("rift.lsp.engine_") =>
        {
            (Code::CapabilityUnavailable, Retry::OperatorAction)
        }
        slug if slug.starts_with("rift.syntax.") => (Code::InternalError, Retry::SameRequest),
        slug if slug.starts_with("rift.core.configuration_") => {
            (Code::ConfigurationInvalid, Retry::OperatorAction)
        }
        _ => return None,
    };
    Some(guidance)
}

fn wire_limit(error: &RiftError, code: wire::ErrorCode) -> Option<wire::LimitEvidence> {
    if code != wire::ErrorCode::LimitExceeded {
        return None;
    }
    let fields = error
        .context()
        .collect::<std::collections::BTreeMap<_, _>>();
    let field = fields.get("field").or_else(|| fields.get("limit"))?.clone();
    let limit = fields
        .get("limit")
        .and_then(|value| value.parse().ok())
        .or_else(|| fields.get("maximum").and_then(|value| value.parse().ok()))
        .or_else(|| fields.get("bytes_max").and_then(|value| value.parse().ok()))
        .or_else(|| {
            fields
                .get("entries_max")
                .and_then(|value| value.parse().ok())
        })
        .or_else(|| fields.get("tags_max").and_then(|value| value.parse().ok()))?;
    let required = fields
        .get("required")
        .and_then(|value| value.parse().ok())
        .or_else(|| fields.get("observed").and_then(|value| value.parse().ok()))
        .or_else(|| fields.get("size").and_then(|value| value.parse().ok()))
        .or_else(|| {
            fields
                .get("announced_bytes")
                .and_then(|value| value.parse().ok())
        })?;
    Some(wire::LimitEvidence {
        field,
        limit,
        required,
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use rift_core::SourceVisibility;
    use rift_error::{ErrorContext, ErrorSlug, ErrorValue, RiftError, errors};
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::error as wire;
    use rift_server::ReadService;
    use rmcp::ErrorData;

    use super::{McpErrorExt as _, WireFailure};

    #[test]
    fn every_registered_error_has_wire_guidance() {
        for slug in errors::REGISTERED_SLUGS {
            assert!(
                super::wire_guidance(slug).is_some(),
                "unmapped error slug: {slug}"
            );
        }
    }

    #[test]
    fn exact_wire_guidance_names_each_error_once() {
        let mut mapped = std::collections::BTreeSet::new();
        for (_, _, slugs) in super::WIRE_GUIDANCE {
            assert!(!slugs.is_empty(), "wire guidance group must name errors");
            for slug in *slugs {
                assert!(mapped.insert(*slug), "duplicate wire guidance: {slug}");
            }
        }
    }

    #[test]
    fn exact_wire_guidance_precedes_namespace_defaults() {
        use wire::{ErrorCode as Code, RetryDirective as Retry};

        for (slug, expected) in [
            (
                "rift.analysis.package_retained_source_limits_invalid",
                (Code::ConfigurationInvalid, Retry::OperatorAction),
            ),
            (
                "rift.analysis.package_retained_source_bytes_exceeded",
                (Code::LimitExceeded, Retry::Never),
            ),
            (
                "rift.lsp.uri_outside_root",
                (Code::PermissionDenied, Retry::Never),
            ),
            (
                "rift.lsp.engine_timed_out",
                (Code::TemporarilyUnavailable, Retry::SameRequest),
            ),
            (
                "rift.core.configuration_unreadable",
                (Code::StorageFailure, Retry::SameRequest),
            ),
            (
                "rift.syntax.source_too_large",
                (Code::LimitExceeded, Retry::Never),
            ),
            (
                "rift.syntax.parse_cancelled",
                (Code::Cancelled, Retry::SameRequest),
            ),
            (
                "rift.tracing.log_queue_limit",
                (Code::ConfigurationInvalid, Retry::OperatorAction),
            ),
            (
                "rift.tracing.log_stream_unavailable",
                (Code::CapabilityUnavailable, Retry::OperatorAction),
            ),
            (
                "rift.tracing.log_subscription_limit",
                (Code::LimitExceeded, Retry::OperatorAction),
            ),
        ] {
            assert_eq!(super::wire_guidance(slug), Some(expected), "{slug}");
        }
    }

    #[test]
    fn wire_guidance_preserves_namespace_defaults_and_unknown_errors() {
        use wire::{ErrorCode as Code, RetryDirective as Retry};

        for (slug, expected) in [
            (
                "rift.lsp.uri_unregistered",
                (Code::UnsupportedPath, Retry::Never),
            ),
            (
                "rift.core.path_unregistered",
                (Code::UnsupportedPath, Retry::Never),
            ),
            (
                "rift.lsp.engine_unregistered",
                (Code::CapabilityUnavailable, Retry::OperatorAction),
            ),
            (
                "rift.lsp.capabilities_unregistered",
                (Code::CapabilityUnavailable, Retry::OperatorAction),
            ),
            (
                "rift.lsp.framing_unregistered",
                (Code::CapabilityUnavailable, Retry::OperatorAction),
            ),
            (
                "rift.syntax.unregistered",
                (Code::InternalError, Retry::SameRequest),
            ),
            (
                "rift.core.configuration_unregistered",
                (Code::ConfigurationInvalid, Retry::OperatorAction),
            ),
        ] {
            assert_eq!(super::wire_guidance(slug), Some(expected), "{slug}");
        }
        for slug in ["", "rift.test.mcp_source", "rift.lsp.engine", "rift.syntax"] {
            assert_eq!(super::wire_guidance(slug), None, "{slug}");
        }
    }

    #[test]
    fn registered_error_projects_to_typed_wire_data() {
        let error = errors::ranking::query_empty().mcp();
        let data = error.wire_error(wire::ErrorPhase::Read);
        assert_eq!(data.code, wire::ErrorCode::InvalidRequest);
        assert_eq!(data.retry, wire::RetryDirective::Never);
        assert_eq!(
            data.message,
            "query is empty; provide query text and resend the request"
        );
    }

    #[test]
    fn unavailable_content_keeps_wire_guidance_and_real_source() {
        let engine_answer = errors::server::read_engine_answer()
            .operation("engine references")
            .detail("character out of range")
            .error();
        assert_eq!(
            engine_answer.to_string(),
            "the addressed content exists but its bytes cannot be served: operation engine references, detail character out of range; read the request again once the engine has read the served revision"
        );

        for error in [
            engine_answer,
            errors::server::read_source_unavailable()
                .path("src/lib.rs")
                .error(),
        ] {
            let data = super::McpFailure::new(error).wire_error(wire::ErrorPhase::Read);
            assert_eq!(data.code, wire::ErrorCode::ContentUnavailable);
            assert_eq!(data.retry, wire::RetryDirective::Never);
        }

        let source = errors::core::configuration_unreadable()
            .file("rift.toml")
            .path("rift.toml")
            .io("permission denied")
            .source(std::io::Error::other("permission denied"))
            .error();
        let data = super::McpFailure::new(source).wire_error(wire::ErrorPhase::Read);
        assert_eq!(data.causes.len(), 1);
        assert_eq!(data.causes[0].message, "permission denied");
    }

    #[test]
    fn immediate_mcp_failure_uses_phase_and_wire_classification() {
        use super::McpErrorFailExt as _;

        let result: Result<(), ErrorData> = errors::ranking::query_empty()
            .mcp()
            .tool_error(wire::ErrorPhase::Read)
            .fail();
        let error = result.expect_err("registered failure must terminate the tool call");
        let data: wire::ErrorData =
            serde_json::from_value(error.data.expect("MCP response retains typed wire data"))
                .expect("MCP response carries wire error data");
        assert_eq!(data.code, wire::ErrorCode::InvalidRequest);
        assert_eq!(data.phase, wire::ErrorPhase::Read);
    }

    #[test]
    fn index_bound_projects_generated_evidence_to_wire_limit() {
        let error = errors::index::lexical_unit_limit()
            .field("units_max")
            .maximum(10_u64)
            .observed(12_u64)
            .error()
            .mcp();
        let limit = error
            .wire_error(wire::ErrorPhase::Read)
            .limit
            .expect("registered bound carries wire limit evidence");
        assert_eq!(limit.field, "units_max");
        assert_eq!(limit.limit, 10);
        assert_eq!(limit.required, 12);
    }

    #[test]
    fn wire_code_for_registered_slug_uses_existing_code_spelling() {
        assert_eq!(
            super::wire_code_for_slug("rift.ranking.query_empty").as_deref(),
            Some("invalid_request")
        );
    }

    #[test]
    fn workspace_syntax_and_history_preserve_child_wire_guidance() {
        let syntax_limit = errors::index::workspace_syntax()
            .cause(
                errors::syntax::source_too_large()
                    .source_bytes(2_u64)
                    .source_bytes_max(1_u64)
                    .error(),
            )
            .error();
        let failure = super::McpFailure::new(syntax_limit);
        let wire = failure.wire_error(wire::ErrorPhase::Read);
        assert_eq!(wire.code, wire::ErrorCode::LimitExceeded);
        assert_eq!(wire.retry, wire::RetryDirective::Never);
        assert_eq!(failure.wire_code(), "limit_exceeded");
        assert_eq!(wire.causes[0].code, wire::ErrorCode::LimitExceeded);

        let syntax_zero = errors::index::workspace_syntax()
            .cause(errors::syntax::zero_limit().bound("source_bytes").error())
            .error();
        let wire = super::McpFailure::new(syntax_zero).wire_error(wire::ErrorPhase::Read);
        assert_eq!(wire.code, wire::ErrorCode::ConfigurationInvalid);
        assert_eq!(wire.retry, wire::RetryDirective::OperatorAction);

        let history = errors::index::workspace_history()
            .cause(
                errors::history::revision_unknown()
                    .rev("missing")
                    .requires("commit")
                    .error(),
            )
            .error();
        let wire = super::McpFailure::new(history).wire_error(wire::ErrorPhase::Read);
        assert_eq!(wire.code, wire::ErrorCode::ResourceNotFound);
        assert_eq!(wire.retry, wire::RetryDirective::OperatorAction);

        let provider = errors::index::workspace_provider()
            .cause(
                errors::syntax::source_too_large()
                    .source_bytes(2_u64)
                    .source_bytes_max(1_u64)
                    .error(),
            )
            .error();
        let wire = super::McpFailure::new(provider).wire_error(wire::ErrorPhase::Read);
        assert_eq!(wire.code, wire::ErrorCode::InternalError);
        assert_eq!(wire.retry, wire::RetryDirective::SameRequest);
    }

    #[derive(Debug)]
    struct Link {
        depth: usize,
        inner: Option<Box<Link>>,
    }

    impl std::fmt::Display for Link {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "link {}", self.depth)
        }
    }

    impl Error for Link {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.inner
                .as_deref()
                .map(|link| link as &(dyn Error + 'static))
        }
    }

    #[test]
    fn cause_walk_stops_at_the_declared_bound() {
        let mut chained = Link {
            depth: 0,
            inner: None,
        };
        for depth in 1..=super::ERROR_CAUSES_MAX + 2 {
            chained = Link {
                depth,
                inner: Some(Box::new(chained)),
            };
        }
        let causes = super::bounded_causes(
            wire::ErrorCode::StorageFailure,
            wire::RetryDirective::Never,
            Some(&chained),
        );
        assert_eq!(
            causes.len(),
            super::ERROR_CAUSES_MAX,
            "a chain deeper than the bound must truncate at the bound"
        );
    }

    #[test]
    fn wire_causes_walk_the_source_chain_with_inherited_classification() {
        let error = ReadService::build(
            std::path::Path::new("not-a-real-rift-workspace"),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect_err("missing root must fail");
        let causes = (&error).mcp().wire_error(wire::ErrorPhase::Read).causes;
        assert!(!causes.is_empty(), "sourced failure must yield causes");
        assert!(causes.len() <= super::ERROR_CAUSES_MAX);
        let code = super::wire_guidance(error.slug().as_str())
            .expect("read failure has registered wire guidance")
            .0;
        for cause in &causes {
            assert!(!cause.message.is_empty(), "cause message must be rendered");
            assert_eq!(cause.code, code);
        }
    }

    #[test]
    fn mcp_failure_forwards_display_and_source_without_added_cause_level() {
        let error = RiftError::new(
            ErrorSlug::new("rift.test.mcp_source"),
            "registered failure",
            "retry the operation",
            vec![ErrorContext::new(
                "source",
                ErrorValue::source(std::io::Error::other("socket gone")),
            )],
        );
        let failure = super::McpFailure::new(error);

        assert_eq!(
            failure.to_string(),
            "registered failure: source socket gone; retry the operation"
        );
        let source = Error::source(&failure).expect("MCP failure forwards registered source");
        assert_eq!(
            source
                .downcast_ref::<std::io::Error>()
                .expect("forwarding keeps concrete source type")
                .to_string(),
            "socket gone"
        );
        assert_eq!(rift_error::causes(&failure), ["socket gone"]);
    }

    #[test]
    fn mcp_failure_debug_keeps_sensitive_evidence_redacted() {
        let error = RiftError::new(
            ErrorSlug::new("rift.test.mcp_sensitive"),
            "registered failure {token}",
            "rotate {token}",
            vec![ErrorContext::with_flags(
                "token",
                "private-token",
                true,
                true,
            )],
        );
        let failure = super::McpFailure::new(error);

        assert!(!format!("{failure:?}").contains("private-token"));
        assert!(!failure.to_string().contains("private-token"));
    }
}
