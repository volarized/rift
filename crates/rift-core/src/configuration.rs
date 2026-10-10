//! Configuration error conversion and policy values `rift-index` reads.
//!
//! The `rift.toml` model lives in `rift-protocol`, below this crate, so its
//! errors are represented by registered Rift errors in this module. It also
//! holds [`SourceVisibility`]: `rift-index` has no dependency on
//! `rift-protocol`, so the workspace's `[source]` table is translated into
//! this plain value here, beside the wire type it comes from. For the same
//! reason it passes on [`is_absolute_program`], the classifier acceptance
//! refuses a configured program with, to the engine launch in `rift-lsp`.

pub use rift_protocol::configuration::is_absolute_program;
use rift_protocol::configuration::{
    EXCLUDED_LOCKFILES_DEFAULT, LanguageConfiguration, LargeFileStrategy, WorkspaceConfiguration,
};
use rift_protocol::documentation::DocumentationConfiguration;
use rift_protocol::source::SourceConfiguration;

use rift_error::errors::core::{
    configuration_package_registry_invalid, configuration_package_selector_invalid,
    configuration_port_selection_conflict,
};
use rift_error::{ErrorContext, RiftError, errors};
use rift_protocol::configuration::ConfigurationViolation::{
    self, PackageRegistryInvalid, PackageSelectorInvalid, PortSelectionConflict,
};
#[cfg(test)]
use rift_protocol::configuration::UnitParseError;

/// Which files below a workspace root the index may see: the resolved
/// `[source]` policy, independent of the wire model it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceVisibility {
    include: Vec<String>,
    exclude: Vec<String>,
    force_include: Vec<String>,
    respect_gitignore: bool,
}

impl SourceVisibility {
    /// Builds one visibility policy from its three switches, reaching past `.gitignore`
    /// nowhere. [`Self::with_force_include`] adds the globs that reach past it.
    #[must_use]
    pub const fn new(include: Vec<String>, exclude: Vec<String>, respect_gitignore: bool) -> Self {
        Self {
            include,
            exclude,
            force_include: Vec::new(),
            respect_gitignore,
        }
    }

    /// Sets the globs whose matches stay visible although `.gitignore` hides them.
    #[must_use]
    pub fn with_force_include(mut self, force_include: Vec<String>) -> Self {
        self.force_include = force_include;
        self
    }

    /// Patterns a file must match to stay visible; empty includes every file.
    #[must_use]
    pub fn include(&self) -> &[String] {
        &self.include
    }

    /// Patterns that drop a file from visibility.
    #[must_use]
    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    /// Patterns whose matches stay visible although `.gitignore` hides them.
    #[must_use]
    pub fn force_include(&self) -> &[String] {
        &self.force_include
    }

    /// Whether the workspace's own `.gitignore` chain hides matching files.
    #[must_use]
    pub const fn respect_gitignore(&self) -> bool {
        self.respect_gitignore
    }
}

impl Default for SourceVisibility {
    /// Every file included, none excluded, `.gitignore` respected.
    fn default() -> Self {
        Self::new(Vec::new(), Vec::new(), true)
    }
}

impl From<&SourceConfiguration> for SourceVisibility {
    fn from(source: &SourceConfiguration) -> Self {
        let patterns = |list: &[rift_protocol::read::PathPattern]| {
            list.iter().map(|pattern| pattern.0.clone()).collect()
        };
        Self::new(
            patterns(&source.include),
            patterns(&source.exclude),
            source.respect_gitignore,
        )
        .with_force_include(patterns(&source.force_include))
    }
}

/// One exact language's resolved path selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageFileSelection {
    identity: String,
    enabled: bool,
    include: Option<Vec<String>>,
    exclude: Vec<String>,
    stdlib: bool,
}

impl LanguageFileSelection {
    /// Language identity in `name` or `name:dialect` form.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Whether matched files receive language service.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Replacement patterns, or absence when shipped patterns apply.
    #[must_use]
    pub fn include(&self) -> Option<&[String]> {
        self.include.as_deref()
    }

    /// Patterns removed from this language's effective matches.
    #[must_use]
    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    /// Whether the dependency context names this language's standard library for the
    /// paths it matches.
    #[must_use]
    pub const fn stdlib(&self) -> bool {
        self.stdlib
    }

    fn from_entry(identity: &str, configuration: &LanguageConfiguration) -> Self {
        let patterns = |list: &[rift_protocol::read::PathPattern]| {
            list.iter().map(|pattern| pattern.0.clone()).collect()
        };
        Self {
            identity: identity.to_owned(),
            enabled: configuration.enabled,
            include: configuration.include.as_deref().map(patterns),
            exclude: patterns(&configuration.exclude),
            stdlib: configuration.stdlib,
        }
    }
}

/// Resolved language path selections in identity order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LanguageFileSelections {
    entries: Vec<LanguageFileSelection>,
    frameworks: Vec<rift_protocol::configuration::SyntaxFrameworkConfiguration>,
}

impl LanguageFileSelections {
    /// Exact language entries in identity order.
    #[must_use]
    pub fn entries(&self) -> &[LanguageFileSelection] {
        &self.entries
    }

    /// Explicit framework context inherited from `[providers.syntax]`.
    #[must_use]
    pub fn frameworks(&self) -> &[rift_protocol::configuration::SyntaxFrameworkConfiguration] {
        &self.frameworks
    }
}

impl From<&WorkspaceConfiguration> for LanguageFileSelections {
    fn from(configuration: &WorkspaceConfiguration) -> Self {
        Self {
            entries: configuration
                .languages
                .iter()
                .map(|(identity, entry)| LanguageFileSelection::from_entry(identity, entry))
                .collect(),
            frameworks: configuration.providers.syntax.frameworks.clone(),
        }
    }
}

/// Resolved `[search.text]` path selection, chunk bound, large-file strategy, and the
/// lockfiles search leaves out, beside the `[documentation]` table deciding which of the
/// text files the index reads it collects as documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFileInclusion {
    include: Vec<String>,
    chunk_bytes_max: u64,
    documentation: DocumentationConfiguration,
    large_files: LargeFileStrategy,
    excluded_lockfiles: Vec<String>,
}

impl TextFileInclusion {
    /// Builds one text-file policy from its patterns and chunk bound, collecting
    /// documentation under the `[documentation]` defaults, splitting a file past the bound
    /// into chunks, and leaving out the lockfiles `[search.text].excluded_lockfiles` names
    /// when the keys are absent.
    #[must_use]
    pub fn new(include: Vec<String>, chunk_bytes_max: u64) -> Self {
        Self {
            include,
            chunk_bytes_max,
            documentation: DocumentationConfiguration::default(),
            large_files: LargeFileStrategy::default(),
            excluded_lockfiles: EXCLUDED_LOCKFILES_DEFAULT
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        }
    }

    /// This policy collecting documentation under `documentation` instead of the defaults.
    #[must_use]
    pub fn with_documentation(mut self, documentation: DocumentationConfiguration) -> Self {
        self.documentation = documentation;
        self
    }

    /// The same policy, leaving out of search the lockfiles `names` names instead.
    #[must_use]
    pub fn excluding_lockfiles(mut self, names: Vec<String>) -> Self {
        self.excluded_lockfiles = names;
        self
    }

    /// The file names of the lockfiles search leaves out.
    #[must_use]
    pub fn excluded_lockfiles(&self) -> &[String] {
        &self.excluded_lockfiles
    }

    /// The same policy, treating a file past the chunk bound as `strategy` decides.
    #[must_use]
    pub const fn with_large_files(mut self, strategy: LargeFileStrategy) -> Self {
        self.large_files = strategy;
        self
    }

    /// What the text index does with a file past the chunk bound.
    #[must_use]
    pub const fn large_files(&self) -> LargeFileStrategy {
        self.large_files
    }

    /// Whether the text index leaves out a text of `content_bytes` bytes: only under
    /// `skip`, and only past the chunk bound.
    #[must_use]
    pub fn skips(&self, content_bytes: usize) -> bool {
        self.large_files == LargeFileStrategy::Skip
            && u64::try_from(content_bytes).unwrap_or(u64::MAX) > self.chunk_bytes_max
    }

    /// Patterns selecting plain text when no language claims a path.
    #[must_use]
    pub fn include(&self) -> &[String] {
        &self.include
    }

    /// Bytes one lexical chunk derived from baseline text may hold.
    #[must_use]
    pub const fn chunk_bytes_max(&self) -> u64 {
        self.chunk_bytes_max
    }

    /// Which text files the index collects as documentation.
    #[must_use]
    pub const fn documentation(&self) -> &DocumentationConfiguration {
        &self.documentation
    }
}

impl Default for TextFileInclusion {
    /// Uses the default `[search.text]` chunk bound and `[documentation]` table.
    fn default() -> Self {
        Self::from(&WorkspaceConfiguration::default())
    }
}

impl From<&WorkspaceConfiguration> for TextFileInclusion {
    fn from(configuration: &WorkspaceConfiguration) -> Self {
        let text = &configuration.search.text;
        Self::new(
            text.include
                .iter()
                .map(|pattern| pattern.0.clone())
                .collect(),
            text.max_chunk.bytes(),
        )
        .with_documentation(configuration.documentation.clone())
        .with_large_files(text.large_files)
        .excluding_lockfiles(text.excluded_lockfiles.clone())
    }
}

#[cfg(test)]
pub(crate) fn unit_parse_error(error: &UnitParseError) -> RiftError {
    errors::core::configuration_unit_parse()
        .value(error.value())
        .expected(error.expected())
        .error()
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::configuration::{ByteSize, Duration};
    use rift_protocol::read::PathPattern;

    #[test]
    fn test_source_visibility_converts_from_wire_configuration() {
        let source = SourceConfiguration {
            include: vec![PathPattern("src/**".to_owned())],
            exclude: vec![PathPattern("src/generated/**".to_owned())],
            force_include: vec![PathPattern("notes/**".to_owned())],
            respect_gitignore: false,
            ..SourceConfiguration::default()
        };
        let visibility = SourceVisibility::from(&source);
        assert_eq!(visibility.include(), ["src/**"]);
        assert_eq!(visibility.exclude(), ["src/generated/**"]);
        assert_eq!(visibility.force_include(), ["notes/**"]);
        assert!(!visibility.respect_gitignore());
    }

    #[test]
    fn test_text_file_inclusion_converts_from_wire_configuration() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.search.text.max_chunk = ByteSize::from_bytes(2 << 20);
        configuration.documentation.enabled = false;
        configuration.documentation.exclude = vec![PathPattern("docs/internal/**".to_owned())];
        let inclusion = TextFileInclusion::from(&configuration);
        assert_eq!(inclusion.chunk_bytes_max(), 2 << 20);
        assert_eq!(inclusion.documentation(), &configuration.documentation);
    }

    #[test]
    fn test_text_file_inclusion_skips_only_past_the_chunk_bound_under_skip() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.search.text.max_chunk = ByteSize::from_bytes(1 << 10);
        let split = TextFileInclusion::from(&configuration);
        assert_eq!(split.large_files(), LargeFileStrategy::Split);
        assert!(!split.skips(4 << 10), "split never leaves a file out");
        configuration.search.text.large_files = LargeFileStrategy::Skip;
        let skip = TextFileInclusion::from(&configuration);
        assert!(!skip.skips(1 << 10), "a text at the bound stays");
        assert!(skip.skips((1 << 10) + 1), "a text past the bound leaves");
    }

    #[test]
    fn test_text_file_inclusion_default_matches_default_configuration() {
        let inclusion = TextFileInclusion::default();
        assert_eq!(inclusion.chunk_bytes_max(), 1 << 20);
        assert!(inclusion.documentation().enabled);
        assert_eq!(
            inclusion,
            TextFileInclusion::from(&WorkspaceConfiguration::default())
        );
        assert_eq!(
            TextFileInclusion::new(Vec::new(), 1 << 20).documentation(),
            &DocumentationConfiguration::default()
        );
    }

    #[test]
    fn test_source_visibility_default_includes_everything_and_respects_gitignore() {
        let visibility = SourceVisibility::default();
        assert!(visibility.include().is_empty());
        assert!(visibility.exclude().is_empty());
        assert!(visibility.force_include().is_empty());
        assert!(visibility.respect_gitignore());
    }

    #[test]
    fn test_unit_parse_failure_renders_through_the_registry() {
        let fault = ByteSize::parse("16KiB").expect_err("an uppercase unit must be refused");
        let error = unit_parse_error(&fault);
        assert_eq!(error.slug().as_str(), "rift.core.configuration_unit_parse");
        let message = error.to_string();
        assert!(
            message.contains("configuration value does not use its required unit form")
                && error
                    .context()
                    .any(|(key, value)| key == "value" && value == "16KiB")
                && error
                    .context()
                    .any(|(key, value)| key == "expected" && value.contains("16kb"))
                && message.contains("correct the reported configuration field"),
            "the render must carry explanation, evidence, and action: {message}"
        );
    }

    #[test]
    fn test_configuration_violation_renders_through_the_registry() {
        let violation = ConfigurationViolation::CommandProgramAbsolute {
            field: "languages.rust.lsp.command",
            program: "/bin/cargo".to_owned(),
        };
        let error = configuration_violation_error(&violation);
        assert_eq!(
            error.slug().as_str(),
            "rift.core.configuration_command_program_absolute"
        );
        let message = error.to_string();
        assert!(
            message.contains("configured command executable is an absolute path")
                && error
                    .context()
                    .any(|(key, value)| key == "field" && value == "languages.rust.lsp.command")
                && error
                    .context()
                    .any(|(key, value)| key == "program" && value == "/bin/cargo")
                && message.contains("correct the reported configuration field"),
            "the render must carry the serde label, the evidence, and the action: {message}"
        );
    }

    #[test]
    fn test_configuration_violations_preserve_registered_identity_and_evidence() {
        // Issue #535: every protocol violation retains its registered error and evidence.
        struct Case {
            violation: ConfigurationViolation,
            slug: &'static str,
            message: &'static str,
            evidence: &'static [(&'static str, &'static str)],
        }
        let cases = [
            Case {
                violation: ConfigurationViolation::LimitOutOfRange {
                    field: "source.files",
                    value: 2,
                    min: 3,
                    max: 9,
                },
                slug: "rift.core.configuration_limit_out_of_range",
                message: "numeric configuration value is outside its documented range",
                evidence: &[
                    ("field", "source.files"),
                    ("value", "2"),
                    ("range", "3..=9"),
                ],
            },
            Case {
                violation: ConfigurationViolation::LanguageIdentityInvalid {
                    language: "Rust".to_owned(),
                },
                slug: "rift.core.configuration_language_identity_invalid",
                message: "language table key is not a canonical language identity",
                evidence: &[("language", "Rust")],
            },
            Case {
                violation: ConfigurationViolation::LanguageLspUnknown {
                    language: "rust".to_owned(),
                    lsp: "missing".to_owned(),
                },
                slug: "rift.core.configuration_language_lsp_unknown",
                message: "language names an LSP process that is not declared",
                evidence: &[("language", "rust"), ("lsp", "missing")],
            },
            Case {
                violation: ConfigurationViolation::LanguageIncludeDuplicate {
                    pattern: "src/**".to_owned(),
                    first: "rust".to_owned(),
                    second: "python".to_owned(),
                },
                slug: "rift.core.configuration_language_include_duplicate",
                message: "two language entries use same include pattern",
                evidence: &[
                    ("pattern", "src/**"),
                    ("first", "rust"),
                    ("second", "python"),
                ],
            },
            Case {
                violation: ConfigurationViolation::EmbeddingModelInvalid {
                    field: "search.vector.embedding.model",
                    value: String::new(),
                },
                slug: "rift.core.configuration_embedding_model_invalid",
                message: "embedding model value does not match its configured kind",
                evidence: &[("field", "search.vector.embedding.model"), ("value", "")],
            },
            Case {
                violation: ConfigurationViolation::EmbeddingEndpointInvalid {
                    field: "search.vector.embedding.endpoint",
                    value: "relative".to_owned(),
                },
                slug: "rift.core.configuration_embedding_endpoint_invalid",
                message: "embedding endpoint is not an accepted URL",
                evidence: &[
                    ("field", "search.vector.embedding.endpoint"),
                    ("value", "relative"),
                ],
            },
            Case {
                violation: ConfigurationViolation::EmbeddingIdentifierInvalid {
                    field: "search.vector.embedding.revision",
                    value: String::new(),
                },
                slug: "rift.core.configuration_embedding_identifier_invalid",
                message: "embedding identifier value is empty or too long",
                evidence: &[("field", "search.vector.embedding.revision"), ("value", "")],
            },
            Case {
                violation: ConfigurationViolation::SearchWeightsInvalid {
                    identifier: 0.0,
                    lexical: 0.0,
                    vector: 0.0,
                },
                slug: "rift.core.configuration_search_weights_invalid",
                message: "search ranking weights are not a usable set",
                evidence: &[
                    ("identifier_weight", "0"),
                    ("lexical_weight", "0"),
                    ("vector_weight", "0"),
                ],
            },
            Case {
                violation: ConfigurationViolation::CommandProgramEmpty {
                    field: "lsp.rust.command",
                },
                slug: "rift.core.configuration_command_program_empty",
                message: "configured command has no executable",
                evidence: &[("field", "lsp.rust.command")],
            },
            Case {
                violation: ConfigurationViolation::CommandProgramWhitespace {
                    field: "lsp.rust.command",
                    program: "cargo check".to_owned(),
                },
                slug: "rift.core.configuration_command_program_whitespace",
                message: "configured command executable contains whitespace",
                evidence: &[("field", "lsp.rust.command"), ("program", "cargo check")],
            },
            Case {
                violation: ConfigurationViolation::CommandProgramAbsolute {
                    field: "lsp.rust.command",
                    program: "/bin/cargo".to_owned(),
                },
                slug: "rift.core.configuration_command_program_absolute",
                message: "configured command executable is an absolute path",
                evidence: &[("field", "lsp.rust.command"), ("program", "/bin/cargo")],
            },
            Case {
                violation: ConfigurationViolation::CommandProgramDotSegment {
                    field: "lsp.rust.command",
                    program: "../cargo".to_owned(),
                },
                slug: "rift.core.configuration_command_program_dot_segment",
                message: "configured command executable contains a dot segment",
                evidence: &[("field", "lsp.rust.command"), ("program", "../cargo")],
            },
            Case {
                violation: ConfigurationViolation::CommandArgumentOversized {
                    field: "lsp.rust.command",
                    bytes: 4097,
                },
                slug: "rift.core.configuration_command_argument_oversized",
                message: "configured command argument exceeds its byte limit",
                evidence: &[
                    ("field", "lsp.rust.command"),
                    ("bytes", "4097"),
                    ("bytes_max", "4096"),
                ],
            },
            Case {
                violation: ConfigurationViolation::CommandProgramOversized {
                    field: "lsp.rust.command",
                    bytes: 4097,
                },
                slug: "rift.core.configuration_command_program_oversized",
                message: "configured command executable exceeds its byte limit",
                evidence: &[
                    ("field", "lsp.rust.command"),
                    ("bytes", "4097"),
                    ("bytes_max", "4096"),
                ],
            },
            Case {
                violation: ConfigurationViolation::LspNameInvalid {
                    name: "Rust".to_owned(),
                },
                slug: "rift.core.configuration_lsp_name_invalid",
                message: "LSP process name is not a lowercase word",
                evidence: &[("name", "Rust")],
            },
            Case {
                violation: ConfigurationViolation::LspEnvironmentKeyInvalid {
                    lsp: "rust".to_owned(),
                    key: "A=B".to_owned(),
                },
                slug: "rift.core.configuration_lsp_environment_key_invalid",
                message: "LSP environment key is empty or contains a forbidden character",
                evidence: &[("lsp", "rust"), ("key", "A=B")],
            },
            Case {
                violation: ConfigurationViolation::LspInitializationOptionsNotObject {
                    lsp: "rust".to_owned(),
                },
                slug: "rift.core.configuration_lsp_initialization_options_not_object",
                message: "LSP initialization options are not a JSON object",
                evidence: &[("lsp", "rust")],
            },
            Case {
                violation: ConfigurationViolation::LspEngineSelectionConflict {
                    lsp: "rust".to_owned(),
                },
                slug: "rift.core.configuration_lsp_engine_selection_conflict",
                message: "LSP table selects command and embedded engines",
                evidence: &[("lsp", "rust"), ("fields", "command, embedded")],
            },
            Case {
                violation: ConfigurationViolation::LspEngineMissing {
                    lsp: "rust".to_owned(),
                },
                slug: "rift.core.configuration_lsp_engine_missing",
                message: "LSP table selects no engine",
                evidence: &[("lsp", "rust"), ("fields", "command, embedded")],
            },
            Case {
                violation: ConfigurationViolation::LspEmbeddedExtras {
                    lsp: "rust".to_owned(),
                    field: "environment",
                },
                slug: "rift.core.configuration_lsp_embedded_extras",
                message: "embedded LSP engine has a spawned-process setting",
                evidence: &[("lsp", "rust"), ("field", "environment")],
            },
            Case {
                violation: ConfigurationViolation::PathPatternInvalid {
                    field: "source.include",
                    pattern: "../src/**".to_owned(),
                },
                slug: "rift.core.configuration_path_pattern_invalid",
                message: "configuration path pattern breaks forward-slash path rules",
                evidence: &[("field", "source.include"), ("pattern", "../src/**")],
            },
            Case {
                violation: ConfigurationViolation::FileNameInvalid {
                    field: "search.text.excluded_lockfiles",
                    name: "dir/Cargo.lock".to_owned(),
                },
                slug: "rift.core.configuration_file_name_invalid",
                message: "excluded lockfile value is not one file name",
                evidence: &[
                    ("field", "search.text.excluded_lockfiles"),
                    ("name", "dir/Cargo.lock"),
                ],
            },
            Case {
                violation: ConfigurationViolation::PackageSelectorInvalid {
                    field: "dependencies.packages",
                    package: "cargo/serde".to_owned(),
                },
                slug: "rift.core.configuration_package_selector_invalid",
                message: "dependency package has conflicting or missing version selector",
                evidence: &[
                    ("field", "dependencies.packages"),
                    ("package", "cargo/serde"),
                ],
            },
            Case {
                violation: ConfigurationViolation::PackageRegistryInvalid {
                    field: "dependencies.packages.registry",
                    package: "cargo/serde".to_owned(),
                },
                slug: "rift.core.configuration_package_registry_invalid",
                message: "dependency package registry is not a canonical endpoint",
                evidence: &[
                    ("field", "dependencies.packages.registry"),
                    ("package", "cargo/serde"),
                ],
            },
            Case {
                violation: ConfigurationViolation::LogCaptureInvalid {
                    capture: "rift=unknown".to_owned(),
                    detail: "invalid filter directive".to_owned(),
                },
                slug: "rift.core.configuration_log_capture_invalid",
                message: "logs.capture is not a tracing filter directive",
                evidence: &[
                    ("field", "logs.capture"),
                    ("capture", "rift=unknown"),
                    ("detail", "invalid filter directive"),
                ],
            },
            Case {
                violation: ConfigurationViolation::PortSelectionConflict,
                slug: "rift.core.configuration_port_selection_conflict",
                message: "server selects port and port range together",
                evidence: &[("fields", "server.port, server.port_range")],
            },
            Case {
                violation: ConfigurationViolation::PortRangeInverted {
                    min: 9001,
                    max: 9000,
                },
                slug: "rift.core.configuration_port_range_inverted",
                message: "server port range maximum is below minimum",
                evidence: &[("min", "9001"), ("max", "9000")],
            },
            Case {
                violation: ConfigurationViolation::HistoryCpuShareInvalid { share: 1.5 },
                slug: "rift.core.configuration_history_cpu_share_invalid",
                message: "history CPU share is outside its accepted range",
                evidence: &[("field", "providers.history.cpu_share"), ("share", "1.5")],
            },
            Case {
                violation: ConfigurationViolation::HistoryReleasesMissing,
                slug: "rift.core.configuration_history_releases_missing",
                message: "selective history strategy has no release pattern",
                evidence: &[(
                    "fields",
                    "providers.history.strategy, providers.history.releases",
                )],
            },
            Case {
                violation: ConfigurationViolation::HistoryReleasesOutsideSelective,
                slug: "rift.core.configuration_history_releases_outside_selective",
                message: "history release patterns require selective strategy",
                evidence: &[(
                    "fields",
                    "providers.history.strategy, providers.history.releases",
                )],
            },
            Case {
                violation: ConfigurationViolation::HistoryReleasePatternInvalid {
                    pattern: "v[1".to_owned(),
                    detail: "unclosed character class".to_owned(),
                },
                slug: "rift.core.configuration_history_release_pattern_invalid",
                message: "history release pattern is invalid",
                evidence: &[
                    ("field", "providers.history.releases"),
                    ("pattern", "v[1"),
                    ("detail", "unclosed character class"),
                ],
            },
        ];
        for case in cases {
            let error = configuration_violation_error(&case.violation);
            assert_eq!(error.slug().as_str(), case.slug, "{:?}", case.violation);
            assert_eq!(error.message(), case.message, "{}", case.slug);
            assert_eq!(
                error.action(),
                "correct the reported configuration field, then retry",
                "{}",
                case.slug
            );
            let actual: std::collections::BTreeMap<_, _> = error.context().collect();
            let expected = case
                .evidence
                .iter()
                .map(|(key, value)| (*key, (*value).to_owned()))
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(actual, expected, "{}", case.slug);
            assert_eq!(
                error.context().count(),
                case.evidence.len(),
                "{}",
                case.slug
            );
            assert!(std::error::Error::source(&error).is_none(), "{}", case.slug);
        }
    }

    #[test]
    fn test_duration_parse_failure_carries_its_own_expected_form() {
        let fault = Duration::parse("30 s").expect_err("an inner space must be refused");
        let context = unit_parse_error(&fault).context().collect::<Vec<_>>();
        let keys: Vec<&str> = context.iter().map(|(key, _)| *key).collect();
        assert_eq!(keys, ["expected", "value"]);
        let expected = &context[0].1;
        assert!(
            expected.contains("30s"),
            "the expected form must name 30s: {expected}"
        );
    }
}
