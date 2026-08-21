//! The `[source]` table of `rift.toml`: which files below the workspace root the index and
//! reads consider visible.

use crate::configuration::{ConfigurationViolation, first_out_of_range};
use crate::read::PathPattern;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Entries `source.include` or `source.exclude` may hold, at most.
pub const SOURCE_PATTERNS_MAX: usize = 512;

/// The `[source]` table: which files below the workspace root the index and reads consider
/// visible. `.git`, `.rift`, and `target` stay invisible whatever this table says.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceConfiguration {
    /// Exact file paths or globs a file must match to stay visible, in [`PathPattern`] syntax.
    /// Empty admits every file `exclude` and `.gitignore` leave standing.
    #[schemars(length(max = 512))]
    pub include: Vec<PathPattern>,
    /// Exact file paths or globs dropped from visibility, in [`PathPattern`] syntax. A match
    /// here is dropped even when `include` also matches it.
    #[schemars(length(max = 512))]
    pub exclude: Vec<PathPattern>,
    /// Whether the workspace's own `.gitignore` files, root and nested, hide the paths they
    /// match. Global and parent-directory gitignore sources are never read.
    pub respect_gitignore: bool,
}

impl Default for SourceConfiguration {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            respect_gitignore: true,
        }
    }
}

impl SourceConfiguration {
    /// The table's list-length bounds, in key order.
    pub(crate) fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "source.include",
                self.include.len() as u64,
                0,
                SOURCE_PATTERNS_MAX as u64,
            ),
            (
                "source.exclude",
                self.exclude.len() as u64,
                0,
                SOURCE_PATTERNS_MAX as u64,
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::WorkspaceConfiguration;
    use serde_json::json;

    #[test]
    fn test_source_pattern_lists_admit_the_cap_and_refuse_above_it() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.source.include = (0..SOURCE_PATTERNS_MAX)
            .map(|index| PathPattern(format!("src/{index}.rs")))
            .collect();
        assert_eq!(configuration.validate(), Ok(()));

        configuration
            .source
            .include
            .push(PathPattern("one-too-many.rs".to_owned()));
        let violation = configuration
            .validate()
            .expect_err("an include list past the cap must be refused");
        assert!(matches!(
            violation,
            ConfigurationViolation::LimitOutOfRange {
                field: "source.include",
                ..
            }
        ));

        let mut configuration = WorkspaceConfiguration::default();
        configuration.source.exclude = vec![PathPattern("x".to_owned()); SOURCE_PATTERNS_MAX + 1];
        let violation = configuration
            .validate()
            .expect_err("an exclude list past the cap must be refused");
        assert!(matches!(
            violation,
            ConfigurationViolation::LimitOutOfRange {
                field: "source.exclude",
                ..
            }
        ));
    }

    #[test]
    fn test_source_configuration_parses_globs_and_exact_paths() {
        let configuration: WorkspaceConfiguration = serde_json::from_value(json!({
            "source": {
                "include": ["src/**", "README.md"],
                "exclude": ["**/generated/**"],
                "respect_gitignore": false,
            }
        }))
        .expect("glob and exact-path entries must parse");
        assert_eq!(
            configuration.source.include,
            vec![
                PathPattern("src/**".to_owned()),
                PathPattern("README.md".to_owned())
            ]
        );
        assert_eq!(
            configuration.source.exclude,
            vec![PathPattern("**/generated/**".to_owned())]
        );
        assert!(!configuration.source.respect_gitignore);
        assert_eq!(configuration.validate(), Ok(()));
    }
}
