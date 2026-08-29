//! Registry conformance for the configuration model's fault types, and the
//! policy values `rift-index` reads out of it.
//!
//! The `rift.toml` model lives in `rift-protocol`, below this crate, so its
//! fault types cannot implement [`Fault`] where they are defined. This module
//! implements the registry trait over them, giving every configuration
//! refusal the registry's identity, explanation, and rendering. It also
//! holds [`SourceVisibility`]: `rift-index` has no dependency on
//! `rift-protocol`, so the workspace's `[source]` table is translated into
//! this plain value here, beside the wire type it comes from.

use rift_protocol::configuration::{
    ConfigurationViolation, LanguageConfiguration, SearchConfiguration, UnitParseError,
    WorkspaceConfiguration,
};
use rift_protocol::source::SourceConfiguration;

use crate::error::{ErrorContext, ErrorName, Fault, fault_label};
use rift_protocol::error::ErrorCode;

/// Which files below a workspace root the index may see: the resolved
/// `[source]` policy, independent of the wire model it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceVisibility {
    include: Vec<String>,
    exclude: Vec<String>,
    respect_gitignore: bool,
}

impl SourceVisibility {
    /// Builds one visibility policy from its three switches.
    #[must_use]
    pub const fn new(include: Vec<String>, exclude: Vec<String>, respect_gitignore: bool) -> Self {
        Self {
            include,
            exclude,
            respect_gitignore,
        }
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
    }
}

/// One exact language's resolved path selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageFileSelection {
    identity: String,
    enabled: bool,
    include: Option<Vec<String>>,
    exclude: Vec<String>,
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

    fn from_entry(identity: &str, configuration: &LanguageConfiguration) -> Self {
        let patterns = |list: &[rift_protocol::read::PathPattern]| {
            list.iter().map(|pattern| pattern.0.clone()).collect()
        };
        Self {
            identity: identity.to_owned(),
            enabled: configuration.enabled,
            include: configuration.include.as_deref().map(patterns),
            exclude: patterns(&configuration.exclude),
        }
    }
}

/// Resolved language path selections in identity order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LanguageFileSelections {
    entries: Vec<LanguageFileSelection>,
}

impl LanguageFileSelections {
    /// Exact language entries in identity order.
    #[must_use]
    pub fn entries(&self) -> &[LanguageFileSelection] {
        &self.entries
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
        }
    }
}

/// Resolved `[search.text]` path selection and chunk bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFileInclusion {
    include: Vec<String>,
    chunk_bytes_max: u64,
}

impl TextFileInclusion {
    /// Builds one text-file policy from its chunk bound.
    #[must_use]
    pub fn new(chunk_bytes_max: u64) -> Self {
        Self {
            include: vec!["**".to_owned()],
            chunk_bytes_max,
        }
    }

    /// Builds one text-file policy from its patterns and chunk bound.
    #[must_use]
    pub const fn with_include(include: Vec<String>, chunk_bytes_max: u64) -> Self {
        Self {
            include,
            chunk_bytes_max,
        }
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
}

impl Default for TextFileInclusion {
    /// Uses default `[search.text]` chunk bound.
    fn default() -> Self {
        Self::from(&SearchConfiguration::default())
    }
}

impl From<&SearchConfiguration> for TextFileInclusion {
    fn from(search: &SearchConfiguration) -> Self {
        Self::with_include(
            search
                .text
                .include
                .iter()
                .map(|pattern| pattern.0.clone())
                .collect(),
            search.text.max_chunk.bytes(),
        )
    }
}

impl Fault for UnitParseError {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::ConfigurationInvalid)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![
            ErrorContext::new("value", self.value()),
            ErrorContext::new("expected", self.expected()),
        ]
    }
}

impl Fault for ConfigurationViolation {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::ConfigurationInvalid)
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("violation", fault_label(self))];
        context.extend(
            self.evidence()
                .into_iter()
                .map(|(key, value)| ErrorContext::new(key, value)),
        );
        context
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use rift_protocol::configuration::{ByteSize, Duration};
    use rift_protocol::read::PathPattern;

    #[test]
    fn test_source_visibility_converts_from_wire_configuration() {
        let source = SourceConfiguration {
            include: vec![PathPattern("src/**".to_owned())],
            exclude: vec![PathPattern("src/generated/**".to_owned())],
            respect_gitignore: false,
        };
        let visibility = SourceVisibility::from(&source);
        assert_eq!(visibility.include(), ["src/**"]);
        assert_eq!(visibility.exclude(), ["src/generated/**"]);
        assert!(!visibility.respect_gitignore());
    }

    #[test]
    fn test_text_file_inclusion_converts_from_wire_search_configuration() {
        let mut search = rift_protocol::configuration::SearchConfiguration::default();
        search.text.max_chunk = ByteSize::from_bytes(2 << 20);
        let inclusion = TextFileInclusion::from(&search);
        assert_eq!(inclusion.chunk_bytes_max(), 2 << 20);
    }

    #[test]
    fn test_text_file_inclusion_default_matches_default_search_configuration() {
        let inclusion = TextFileInclusion::default();
        assert_eq!(inclusion.chunk_bytes_max(), 1 << 20);
        assert_eq!(
            inclusion,
            TextFileInclusion::from(&rift_protocol::configuration::SearchConfiguration::default())
        );
    }

    #[test]
    fn test_source_visibility_default_includes_everything_and_respects_gitignore() {
        let visibility = SourceVisibility::default();
        assert!(visibility.include().is_empty());
        assert!(visibility.exclude().is_empty());
        assert!(visibility.respect_gitignore());
    }

    #[test]
    fn test_unit_parse_failure_renders_through_the_registry() {
        let fault = ByteSize::parse("16KiB").expect_err("an uppercase unit must be refused");
        let error = Error::from(fault);
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::ConfigurationInvalid)
        );
        let message = error.to_string();
        assert!(
            message.contains("the workspace configuration failed validation")
                && message.contains("value 16KiB")
                && message.contains("16kb")
                && message.contains("correct the reported configuration field"),
            "the render must carry explanation, evidence, and action: {message}"
        );
    }

    #[test]
    fn test_configuration_violation_renders_through_the_registry() {
        let violation = ConfigurationViolation::CommandProgramAbsolute {
            field: "hooks.command",
            program: "/bin/cargo".to_owned(),
        };
        let error = Error::from(violation);
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::ConfigurationInvalid)
        );
        let message = error.to_string();
        assert!(
            message.contains("violation command_program_absolute")
                && message.contains("field hooks.command")
                && message.contains("program /bin/cargo")
                && message.contains("correct the reported configuration field"),
            "the render must carry the serde label, the evidence, and the action: {message}"
        );
    }

    #[test]
    fn test_duration_parse_failure_carries_its_own_expected_form() {
        let fault = Duration::parse("30 s").expect_err("an inner space must be refused");
        let context = fault.context();
        let keys: Vec<&str> = context.iter().map(ErrorContext::key).collect();
        assert_eq!(keys, ["value", "expected"]);
        let expected = context[1].value();
        assert!(
            expected.contains("30s"),
            "the expected form must name 30s: {expected}"
        );
    }
}
