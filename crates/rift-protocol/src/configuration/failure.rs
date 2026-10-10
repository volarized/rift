use super::ConfigurationViolation;
use super::ConfigurationViolation::{
    PackageRegistryInvalid, PackageSelectorInvalid, PortSelectionConflict,
};
use rift_error::errors::core::{
    configuration_package_registry_invalid, configuration_package_selector_invalid,
    configuration_port_selection_conflict,
};
use rift_error::{ErrorContext, RiftError, errors};

/// Converts one accepted configuration violation to its registered error.
#[must_use]
pub fn configuration_violation_error(violation: &ConfigurationViolation) -> RiftError {
    macro_rules! build {
        ($builder:expr) => {{
            let mut builder = $builder;
            for (key, value) in violation.evidence() {
                builder = builder.with(ErrorContext::new(key, value));
            }
            builder.error()
        }};
    }
    match violation {
        ConfigurationViolation::McpRegistrationInvalid { .. } => mcp_error(violation),
        ConfigurationViolation::LimitOutOfRange { .. } => {
            build!(errors::core::configuration_limit_out_of_range())
        }
        ConfigurationViolation::LanguageIdentityInvalid { .. } => {
            build!(errors::core::configuration_language_identity_invalid())
        }
        ConfigurationViolation::LanguageLspUnknown { .. } => {
            build!(errors::core::configuration_language_lsp_unknown())
        }
        ConfigurationViolation::LanguageIncludeDuplicate { .. } => {
            build!(errors::core::configuration_language_include_duplicate())
        }
        ConfigurationViolation::EmbeddingModelInvalid { .. } => {
            build!(errors::core::configuration_embedding_model_invalid())
        }
        ConfigurationViolation::EmbeddingEndpointInvalid { .. } => {
            build!(errors::core::configuration_embedding_endpoint_invalid())
        }
        ConfigurationViolation::EmbeddingIdentifierInvalid { .. } => {
            build!(errors::core::configuration_embedding_identifier_invalid())
        }
        ConfigurationViolation::SearchWeightsInvalid { .. } => {
            build!(errors::core::configuration_search_weights_invalid())
        }
        ConfigurationViolation::CommandProgramEmpty { .. } => {
            build!(errors::core::configuration_command_program_empty())
        }
        ConfigurationViolation::CommandProgramWhitespace { .. } => {
            build!(errors::core::configuration_command_program_whitespace())
        }
        ConfigurationViolation::CommandProgramAbsolute { .. } => {
            build!(errors::core::configuration_command_program_absolute())
        }
        ConfigurationViolation::CommandProgramDotSegment { .. } => {
            build!(errors::core::configuration_command_program_dot_segment())
        }
        ConfigurationViolation::CommandArgumentOversized { .. } => {
            build!(errors::core::configuration_command_argument_oversized())
        }
        ConfigurationViolation::CommandProgramOversized { .. } => {
            build!(errors::core::configuration_command_program_oversized())
        }
        ConfigurationViolation::LspNameInvalid { .. } => {
            build!(errors::core::configuration_lsp_name_invalid())
        }
        ConfigurationViolation::LspEnvironmentKeyInvalid { .. } => {
            build!(errors::core::configuration_lsp_environment_key_invalid())
        }
        ConfigurationViolation::LspInitializationOptionsNotObject { .. } => {
            build!(errors::core::configuration_lsp_initialization_options_not_object())
        }
        ConfigurationViolation::LspEngineSelectionConflict { .. } => {
            build!(errors::core::configuration_lsp_engine_selection_conflict())
        }
        ConfigurationViolation::LspEngineMissing { .. } => {
            build!(errors::core::configuration_lsp_engine_missing())
        }
        ConfigurationViolation::LspEmbeddedExtras { .. } => {
            build!(errors::core::configuration_lsp_embedded_extras())
        }
        ConfigurationViolation::PathPatternInvalid { .. } => {
            build!(errors::core::configuration_path_pattern_invalid())
        }
        ConfigurationViolation::FileNameInvalid { .. } => {
            build!(errors::core::configuration_file_name_invalid())
        }
        PackageSelectorInvalid { .. } => build!(configuration_package_selector_invalid()),
        PackageRegistryInvalid { .. } => build!(configuration_package_registry_invalid()),
        ConfigurationViolation::LogCaptureInvalid { .. } => {
            build!(errors::core::configuration_log_capture_invalid())
        }
        PortSelectionConflict => build!(configuration_port_selection_conflict()),
        ConfigurationViolation::PortRangeInverted { .. } => {
            build!(errors::core::configuration_port_range_inverted())
        }
        ConfigurationViolation::HistoryCpuShareInvalid { .. } => {
            build!(errors::core::configuration_history_cpu_share_invalid())
        }
        ConfigurationViolation::HistoryReleasesMissing => {
            build!(errors::core::configuration_history_releases_missing())
        }
        ConfigurationViolation::HistoryReleasesOutsideSelective => {
            build!(errors::core::configuration_history_releases_outside_selective())
        }
        ConfigurationViolation::HistoryReleasePatternInvalid { .. } => {
            build!(errors::core::configuration_history_release_pattern_invalid())
        }
    }
}

fn mcp_error(violation: &ConfigurationViolation) -> RiftError {
    let mut builder = errors::core::configuration_invalid();
    for (key, value) in violation.evidence() {
        builder = builder.with(ErrorContext::new(key, value));
    }
    builder.error()
}
