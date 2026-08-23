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

use rift_protocol::configuration::{ConfigurationViolation, SearchConfiguration, UnitParseError};
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

/// Which non-source text files the lexical index includes: the resolved `[search.text]`
/// policy, independent of the wire model it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFileInclusion {
    extensions: Vec<String>,
    chunk_bytes_max: u64,
}

impl TextFileInclusion {
    /// Builds one text-inclusion policy from its extension list and chunk bound.
    #[must_use]
    pub const fn new(extensions: Vec<String>, chunk_bytes_max: u64) -> Self {
        Self {
            extensions,
            chunk_bytes_max,
        }
    }

    /// Extensions, without the leading dot, included as text-file lexical units.
    #[must_use]
    pub fn extensions(&self) -> &[String] {
        &self.extensions
    }

    /// Bytes one lexical chunk derived from an included text file may hold.
    #[must_use]
    pub const fn chunk_bytes_max(&self) -> u64 {
        self.chunk_bytes_max
    }

    /// Whether `path`'s extension is one this policy includes. The comparison is
    /// case-insensitive against the configured lowercase spellings — `README.MD` is included
    /// by `extensions = ["md"]`, matching how case-insensitive filesystems already present
    /// extensions to callers — while configuration acceptance still refuses any entry that is
    /// not itself lowercase.
    #[must_use]
    pub fn includes(&self, path: &std::path::Path) -> bool {
        path.extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| {
                self.extensions
                    .iter()
                    .any(|included| included.eq_ignore_ascii_case(extension))
            })
    }
}

impl Default for TextFileInclusion {
    /// Every default `[search.text]` extension included, at the default chunk bound.
    fn default() -> Self {
        Self::from(&SearchConfiguration::default())
    }
}

impl From<&SearchConfiguration> for TextFileInclusion {
    fn from(search: &SearchConfiguration) -> Self {
        Self::new(
            search.text.extensions.clone(),
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
        search.text.extensions = vec!["md".to_owned(), "rst".to_owned()];
        search.text.max_chunk = ByteSize::from_bytes(2 << 20);
        let inclusion = TextFileInclusion::from(&search);
        assert_eq!(inclusion.extensions(), ["md", "rst"]);
        assert_eq!(inclusion.chunk_bytes_max(), 2 << 20);
    }

    #[test]
    fn test_text_file_inclusion_default_matches_default_search_configuration() {
        let inclusion = TextFileInclusion::default();
        assert_eq!(inclusion.extensions(), ["md", "mdx", "txt"]);
        assert_eq!(inclusion.chunk_bytes_max(), 1 << 20);
        assert_eq!(
            inclusion,
            TextFileInclusion::from(&rift_protocol::configuration::SearchConfiguration::default())
        );
    }

    #[test]
    fn test_text_file_inclusion_includes_a_configured_extension_case_insensitively() {
        let inclusion = TextFileInclusion::new(vec!["md".to_owned()], 1_024);
        assert!(inclusion.includes(std::path::Path::new("docs/readme.md")));
        assert!(
            inclusion.includes(std::path::Path::new("docs/README.MD")),
            "a mixed-case filesystem extension must still be included"
        );
        assert!(!inclusion.includes(std::path::Path::new("docs/readme.txt")));
        assert!(!inclusion.includes(std::path::Path::new("docs/no-extension")));
    }

    #[test]
    fn test_text_extension_configuration_still_refuses_uppercase_entries() {
        use rift_protocol::configuration::{ConfigurationViolation, WorkspaceConfiguration};
        let mut configuration = WorkspaceConfiguration::default();
        configuration.search.text.extensions = vec!["MD".to_owned()];
        assert_eq!(
            configuration.validate(),
            Err(ConfigurationViolation::TextExtensionInvalid {
                extension: "MD".to_owned(),
            }),
            "config-side acceptance must still refuse an uppercase extension entry, even though \
             the runtime path-matching predicate is case-insensitive"
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
        let violation = ConfigurationViolation::HookExecutableAbsolute {
            id: "tests".to_owned(),
            program: "/bin/cargo".to_owned(),
        };
        let error = Error::from(violation);
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::ConfigurationInvalid)
        );
        let message = error.to_string();
        assert!(
            message.contains("violation hook_executable_absolute")
                && message.contains("id tests")
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
