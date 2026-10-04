//! Wire projection of operating failures served on tool results.

use rift_error::{IntoRiftError, RiftError};
use rift_protocol::error as wire;
use rmcp::ErrorData;
use rmcp::model::ErrorCode;

/// JSON-RPC error code every Rift operating failure travels under: the
/// first code of the server-defined range (-32000 to -32099), which rmcp
/// exports no constant for - its constants name only MCP-defined codes. The
/// machine-readable classification is the [`wire::ErrorData`] in `data`.
pub(crate) const RIFT_ERROR_CODE: ErrorCode = ErrorCode(-32000);

/// Most `causes` entries one wire error carries, matching the advertised
/// schema bound.
pub(crate) const ERROR_CAUSES_MAX: usize = 8;

/// Boundary view of a read failure: the projection a tool handler serves as
/// the JSON-RPC error object the design documents - code `-32000`, the
/// rendered failure line as `message`, and the typed [`wire::ErrorData`] as
/// `data`.
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
pub struct McpFailure {
    error: RiftError,
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
}

rift_error::format! {
    mcp -> McpFailure as McpErrorExt, McpErrorFailExt using McpFailure::new
}

impl WireFailure for McpFailure {
    fn tool_error(&self, phase: wire::ErrorPhase) -> ErrorData {
        let data = serde_json::to_value(self.wire_error(phase)).ok();
        ErrorData::new(RIFT_ERROR_CODE, self.error.to_string(), data)
    }

    fn wire_error(&self, phase: wire::ErrorPhase) -> wire::ErrorData {
        let (code, retry) = wire_guidance(self.error.slug().as_str()).unwrap_or((
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
        let (code, retry) = wire_guidance(self.error.slug().as_str()).unwrap_or((
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

fn wire_guidance(slug: &str) -> Option<(wire::ErrorCode, wire::RetryDirective)> {
    use wire::{ErrorCode as Code, RetryDirective as Retry};
    let guidance = match slug {
        "rift.ranking.query_empty"
        | "rift.ranking.query_length"
        | "rift.ranking.query_term_length"
        | "rift.ranking.query_quote_unterminated"
        | "rift.ranking.query_phrase_limit"
        | "rift.ranking.pattern_syntax"
        | "rift.ranking.pattern_size"
        | "rift.core.identity_invalid"
        | "rift.core.revision_zero"
        | "rift.core.resolver_id_empty"
        | "rift.core.resolver_id_too_long"
        | "rift.core.resolver_id_invalid_character"
        | "rift.core.contribution_invalid_name"
        | "rift.core.contribution_invalid_language"
        | "rift.core.contribution_invalid_kind"
        | "rift.core.contribution_invalid_source_range"
        | "rift.core.contribution_invalid_origin"
        | "rift.core.contribution_unbound_identity"
        | "rift.core.contribution_too_many_facts"
        | "rift.core.contribution_too_much_evidence"
        | "rift.core.contribution_too_many_namespaced_facts"
        | "rift.core.contribution_invalid_namespace"
        | "rift.core.contribution_invalid_namespace_version"
        | "rift.core.contribution_invalid_reference"
        | "rift.core.contribution_duplicate_fact"
        | "rift.core.contribution_invalid_record"
        | "rift.core.source_unit_id_too_long"
        | "rift.core.source_unit_id_invalid_address"
        | "rift.core.source_unit_id_invalid_resolver"
        | "rift.core.source_unit_id_invalid_encoding"
        | "rift.core.source_unit_id_invalid_key"
        | "rift.core.source_unit_id_non_canonical" => (Code::InvalidRequest, Retry::Never),
        "rift.ranking.reader_failed" | "rift.core.configuration_unreadable" => {
            (Code::StorageFailure, Retry::SameRequest)
        }
        "rift.ranking.document_identity_empty"
        | "rift.ranking.document_field_length"
        | "rift.ranking.capabilities_incompatible" => (Code::InternalError, Retry::SameRequest),
        "rift.ranking.ranking_weights_invalid" | "rift.ranking.fusion_constant_invalid" => {
            (Code::ConfigurationInvalid, Retry::OperatorAction)
        }
        "rift.mcp.parameter_invalid" | "rift.mcp.arguments_not_object" => {
            (Code::InvalidRequest, Retry::Never)
        }
        "rift.mcp.election_storage_failed" => (Code::StorageFailure, Retry::SameRequest),
        "rift.mcp.proxy_initialization_failed"
        | "rift.mcp.http_ports_exhausted"
        | "rift.mcp.forward_unanswered"
        | "rift.mcp.start_building"
        | "rift.mcp.spawn_no_output"
        | "rift.mcp.spawn_failed" => (Code::TemporarilyUnavailable, Retry::SameRequest),
        "rift.mcp.election_already_serving"
        | "rift.mcp.election_document_invalid"
        | "rift.mcp.http_serve_failed"
        | "rift.mcp.proxy_identity_failed"
        | "rift.mcp.proxy_task_failed"
        | "rift.mcp.proxy_unexpected_quit"
        | "rift.mcp.project_hit_identity_missing"
        | "rift.mcp.project_hit_identity_undecodable"
        | "rift.mcp.project_hit_name_unmatched"
        | "rift.mcp.project_hit_unit_invalid"
        | "rift.mcp.project_hit_identity_refused" => (Code::InternalError, Retry::SameRequest),
        "rift.lsp.correlation_pending_requests_exceeded"
        | "rift.lsp.framing_header_too_long"
        | "rift.lsp.framing_message_too_long" => (Code::LimitExceeded, Retry::Never),
        "rift.lsp.position_line_out_of_range"
        | "rift.lsp.position_character_out_of_range"
        | "rift.lsp.position_character_misaligned"
        | "rift.lsp.position_offset_misaligned"
        | "rift.lsp.position_offset_inside_line_ending" => {
            (Code::InternalError, Retry::SameRequest)
        }
        "rift.lsp.uri_outside_root" => (Code::PermissionDenied, Retry::Never),
        "rift.lsp.engine_program_empty" | "rift.lsp.engine_program_absolute" => {
            (Code::ConfigurationInvalid, Retry::OperatorAction)
        }
        "rift.lsp.engine_connection_closed"
        | "rift.lsp.engine_timed_out"
        | "rift.lsp.engine_analyzing"
        | "rift.lsp.engine_ended"
        | "rift.lsp.engine_refused_retryable" => (Code::TemporarilyUnavailable, Retry::SameRequest),
        "rift.lsp.engine_refused_terminal" => (Code::InvalidRequest, Retry::Never),
        slug if slug.starts_with("rift.lsp.uri_") => (Code::UnsupportedPath, Retry::Never),
        slug if slug.starts_with("rift.lsp.capabilities_")
            || slug == "rift.lsp.correlation_response_unknown"
            || slug.starts_with("rift.lsp.framing_")
            || slug.starts_with("rift.lsp.engine_") =>
        {
            (Code::CapabilityUnavailable, Retry::OperatorAction)
        }
        "rift.analysis.package_declarations_exceeded"
        | "rift.analysis.documentation_limit_exceeded"
        | "rift.analysis.package_input_too_many_files"
        | "rift.analysis.package_input_too_many_bytes"
        | "rift.analysis.context7_oversized"
        | "rift.analysis.context7_too_many_entries" => (Code::LimitExceeded, Retry::Never),
        "rift.analysis.documentation_target_missing" => {
            (Code::ResourceNotFound, Retry::OperatorAction)
        }
        "rift.analysis.context7_malformed" | "rift.analysis.context7_entry_invalid" => {
            (Code::ContentUnavailable, Retry::Never)
        }
        "rift.analysis.source_pattern_invalid" => {
            (Code::ConfigurationInvalid, Retry::OperatorAction)
        }
        "rift.analysis.package_identity_invalid"
        | "rift.analysis.package_syntax_unavailable"
        | "rift.analysis.package_provider_failed"
        | "rift.analysis.documentation_identity_invalid"
        | "rift.analysis.documentation_duplicate_source"
        | "rift.analysis.documentation_digest_mismatch"
        | "rift.analysis.documentation_origin_invalid"
        | "rift.analysis.documentation_format_invalid"
        | "rift.analysis.documentation_range_invalid"
        | "rift.analysis.documentation_order_invalid"
        | "rift.analysis.documentation_notebook_invalid"
        | "rift.analysis.documentation_revision_invalid"
        | "rift.analysis.documentation_encoding_failed"
        | "rift.analysis.package_input_identity_invalid"
        | "rift.analysis.package_input_origin_invalid"
        | "rift.analysis.package_input_duplicate_path" => (Code::InvalidRequest, Retry::Never),
        "rift.provider.composition_invalid_name"
        | "rift.provider.composition_duplicate_stage"
        | "rift.provider.composition_foreign_flow"
        | "rift.provider.composition_type_mismatch"
        | "rift.provider.composition_stage_not_found"
        | "rift.provider.composition_dangling_input"
        | "rift.provider.composition_missing_output" => {
            (Code::ConfigurationInvalid, Retry::OperatorAction)
        }
        "rift.provider.cache_too_many_keys" => (Code::LimitExceeded, Retry::Never),
        "rift.provider.publication_zero_limit"
        | "rift.provider.publication_provider_mismatch"
        | "rift.provider.publication_revision_mismatch"
        | "rift.provider.publication_duplicate_symbol"
        | "rift.provider.publication_provider_limit"
        | "rift.provider.publication_provider_contribution_limit"
        | "rift.provider.publication_contribution_limit" => (Code::InvalidRequest, Retry::Never),
        "rift.search.model_file_missing" => (Code::ResourceNotFound, Retry::OperatorAction),
        "rift.search.model_download_failed" => (Code::TemporarilyUnavailable, Retry::SameRequest),
        "rift.search.model_download_too_large" | "rift.search.text_limit" => {
            (Code::LimitExceeded, Retry::Never)
        }
        "rift.search.vector_coordinate_invalid"
        | "rift.search.model_configuration_invalid"
        | "rift.search.model_source_invalid"
        | "rift.search.model_cache_unavailable"
        | "rift.search.tokenizer_unreadable"
        | "rift.search.weights_unreadable"
        | "rift.search.vector_width_mismatch" => {
            (Code::ConfigurationInvalid, Retry::OperatorAction)
        }
        "rift.search.encode_failed" | "rift.search.task_failed" | "rift.search.store_failed" => {
            (Code::InternalError, Retry::SameRequest)
        }
        slug if slug.starts_with("rift.syntax.") => match slug {
            "rift.syntax.zero_limit" => (Code::ConfigurationInvalid, Retry::OperatorAction),
            "rift.syntax.source_too_large"
            | "rift.syntax.too_many_nodes"
            | "rift.syntax.too_deep"
            | "rift.syntax.too_many_captures"
            | "rift.syntax.too_many_markdown_inline_ranges"
            | "rift.syntax.markdown_progress_exceeded" => (Code::LimitExceeded, Retry::Never),
            "rift.syntax.parse_cancelled" => (Code::Cancelled, Retry::SameRequest),
            _ => (Code::InternalError, Retry::SameRequest),
        },
        "rift.index.lexical_storage" | "rift.index.workspace_filesystem" => {
            (Code::StorageFailure, Retry::SameRequest)
        }
        "rift.index.lexical_unit_limit"
        | "rift.index.lexical_unit_too_large"
        | "rift.index.lexical_record_limit"
        | "rift.index.workspace_too_deep"
        | "rift.index.workspace_too_many_files"
        | "rift.index.workspace_file_too_large"
        | "rift.index.workspace_workspace_too_large"
        | "rift.index.workspace_result_limit"
        | "rift.index.workspace_documentation_limit" => (Code::LimitExceeded, Retry::Never),
        "rift.index.workspace_invalid_path" => (Code::UnsupportedPath, Retry::Never),
        "rift.index.workspace_invalid_source" => (Code::ContentUnavailable, Retry::Never),
        "rift.index.workspace_changed_during_capture" => {
            (Code::TemporarilyUnavailable, Retry::SameRequest)
        }
        "rift.index.workspace_cancelled" => (Code::Cancelled, Retry::SameRequest),
        "rift.index.workspace_zero_limit"
        | "rift.index.workspace_invalid_root"
        | "rift.index.workspace_composition"
        | "rift.index.workspace_source_pattern_invalid"
        | "rift.index.workspace_language_include_required"
        | "rift.index.workspace_language_match_conflict" => {
            (Code::ConfigurationInvalid, Retry::OperatorAction)
        }
        "rift.index.workspace_syntax"
        | "rift.index.workspace_provider"
        | "rift.index.workspace_history" => (Code::InternalError, Retry::SameRequest),
        "rift.index.lexical_stored_path_invalid"
        | "rift.index.lexical_stored_kind_invalid"
        | "rift.index.lexical_document_location_unsupported"
        | "rift.index.lexical_duplicate_identity" => (Code::InternalError, Retry::SameRequest),
        "rift.history.unversioned" => (Code::CapabilityUnavailable, Retry::OperatorAction),
        "rift.history.revision_unknown" | "rift.history.revision_not_commit" => {
            (Code::ResourceNotFound, Retry::OperatorAction)
        }
        "rift.history.tree_too_large"
        | "rift.history.blob_too_large"
        | "rift.history.too_many_tags" => (Code::LimitExceeded, Retry::Never),
        "rift.history.path_unrepresentable" => (Code::UnsupportedPath, Retry::Never),
        "rift.history.storage"
        | "rift.history_store.folder"
        | "rift.history_store.database"
        | "rift.server.read_storage" => (Code::StorageFailure, Retry::SameRequest),
        "rift.history_store.lock_unstable"
        | "rift.server.read_unavailable"
        | "rift.server.read_capacity_timeout" => (Code::TemporarilyUnavailable, Retry::SameRequest),
        "rift.server.read_unsupported" | "rift.server.read_unclaimed_extension" => {
            (Code::CapabilityUnavailable, Retry::OperatorAction)
        }
        "rift.server.read_invalid" => (Code::InvalidRequest, Retry::Never),
        "rift.server.read_not_found" => (Code::ResourceNotFound, Retry::Never),
        "rift.server.read_source_unavailable" | "rift.server.read_engine_answer" => {
            (Code::ContentUnavailable, Retry::SameRequest)
        }
        "rift.server.read_task" => (Code::InternalError, Retry::SameRequest),
        "rift.server.read_cancelled" => (Code::Cancelled, Retry::SameRequest),
        "rift.cli.install_home_unresolved"
        | "rift.cli.install_template_missing_tool"
        | "rift.cli.install_write_failed"
        | "rift.cli.install_remove_failed"
        | "rift.cli.install_settings_unparsable"
        | "rift.cli.server_already_serving"
        | "rift.cli.server_spawn_failed"
        | "rift.cli.server_start_exited"
        | "rift.cli.server_start_timed_out"
        | "rift.cli.server_election_unreleased"
        | "rift.cli.server_stop_request_failed"
        | "rift.cli.server_stop_refused"
        | "rift.cli.server_stop_timed_out"
        | "rift.cli.server_logs_unavailable" => (Code::InternalError, Retry::SameRequest),
        slug if slug.starts_with("rift.core.path_") => (Code::UnsupportedPath, Retry::Never),
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
    use rift_error::errors;
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::error as wire;
    use rift_server::ReadService;

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
        let causes = error.wire_causes();
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
}
