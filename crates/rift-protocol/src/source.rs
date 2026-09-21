//! The `[source]` table of `rift.toml`: which files below the workspace root the index and
//! reads consider visible, and how many files, bytes, and declarations the index holds
//! together.

use crate::configuration::{ByteSize, ConfigurationViolation, first_out_of_range};
use crate::read::PathPattern;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Entries `source.include` or `source.exclude` may hold, at most.
pub const SOURCE_PATTERNS_MAX: usize = 512;
/// Files the index may hold, by default.
pub const SOURCE_FILES_DEFAULT: u64 = 100_000;
/// Files the index may hold, at least.
pub const SOURCE_FILES_MIN: u64 = 1_000;
/// Files the index may hold, at most.
pub const SOURCE_FILES_MAX: u64 = 5_000_000;
/// Bytes every indexed file may hold together, by default: 512 MiB.
pub const SOURCE_WORKSPACE_BYTES_DEFAULT: u64 = 512 << 20;
/// Bytes every indexed file may hold together, at least: 16 MiB.
pub const SOURCE_WORKSPACE_BYTES_MIN: u64 = 16 << 20;
/// Bytes every indexed file may hold together, at most: 64 GiB.
pub const SOURCE_WORKSPACE_BYTES_MAX: u64 = 64 << 30;
/// Declarations the index may hold, by default.
///
/// The index costs about 4 KiB per declaration and 25 KiB per file above a 270 MiB base,
/// measured over three real trees, so a workspace of this size holds its index in about
/// 5 GiB. bun v1.4.2 declares 402,812 from 14,654 files and Next.js v16.3.5 declares
/// 439,749 from 26,162, so the default leaves room for a workspace twice either one.
pub const SOURCE_DECLARATIONS_DEFAULT: u64 = 1_000_000;
/// Declarations the index may hold, at least.
pub const SOURCE_DECLARATIONS_MIN: u64 = 10_000;
/// Declarations the index may hold, at most.
pub const SOURCE_DECLARATIONS_MAX: u64 = 50_000_000;
/// The key path acceptance and the index build both name when the file count bound is
/// crossed.
pub const SOURCE_FILES_FIELD: &str = "source.files";
/// The key path acceptance and the index build both name when the aggregate byte bound
/// is crossed.
pub const SOURCE_WORKSPACE_SIZE_FIELD: &str = "source.workspace_size";
/// The key path acceptance names when the declaration bound is crossed, and the key the
/// index build reports a file left out past that bound against.
pub const SOURCE_DECLARATIONS_FIELD: &str = "source.declarations";

/// The `[source]` table: which files below the workspace root the index and reads consider
/// visible, and how many files, bytes, and declarations the index holds together. `.git`,
/// `.rift`, and `target` stay invisible whatever this table says.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_source_ranges)]
pub struct SourceConfiguration {
    /// Exact file paths or globs a file must match to stay visible, in [`PathPattern`] syntax.
    /// Empty includes every file `exclude` and `.gitignore` leave standing.
    #[schemars(length(max = 512))]
    pub include: Vec<PathPattern>,
    /// Exact file paths or globs dropped from visibility, in [`PathPattern`] syntax. A match
    /// here is dropped even when `include` also matches it.
    #[schemars(length(max = 512))]
    pub exclude: Vec<PathPattern>,
    /// Exact file paths or globs that stay visible although the workspace's `.gitignore`
    /// hides them, in [`PathPattern`] syntax. A match here needs no `include` match, and a
    /// match `exclude` also names is still dropped.
    #[schemars(length(max = 512))]
    pub force_include: Vec<PathPattern>,
    /// Whether the workspace's own `.gitignore` files, root and nested, hide the paths they
    /// match. Global and parent-directory gitignore sources are never read.
    pub respect_gitignore: bool,
    /// Most files the index holds, 1000 to 5000000. A workspace past it refuses its rebuild
    /// naming this key.
    #[schemars(range(min = 1_000, max = 5_000_000))]
    #[serde(default = "default_source_files")]
    pub files: u64,
    /// Most source bytes the index holds together, 16mb to 64gb. A workspace past it
    /// refuses its rebuild naming this key.
    #[serde(default = "default_source_workspace_size")]
    pub workspace_size: ByteSize,
    /// Most declarations the index holds together, 10000 to 50000000. A workspace
    /// declaring more publishes the files that fit and leaves the rest out, each one
    /// reported as an unavailable source naming this key.
    #[schemars(range(min = 10_000, max = 50_000_000))]
    #[serde(default = "default_source_declarations")]
    pub declarations: u64,
}

impl Default for SourceConfiguration {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            force_include: Vec::new(),
            respect_gitignore: true,
            files: default_source_files(),
            workspace_size: default_source_workspace_size(),
            declarations: default_source_declarations(),
        }
    }
}

impl SourceConfiguration {
    /// The table's list-length and numeric bounds, then each pattern's forward-slash-only
    /// contract, in key then list order.
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
            (
                SOURCE_FILES_FIELD,
                self.files,
                SOURCE_FILES_MIN,
                SOURCE_FILES_MAX,
            ),
            (
                SOURCE_WORKSPACE_SIZE_FIELD,
                self.workspace_size.bytes(),
                SOURCE_WORKSPACE_BYTES_MIN,
                SOURCE_WORKSPACE_BYTES_MAX,
            ),
            (
                SOURCE_DECLARATIONS_FIELD,
                self.declarations,
                SOURCE_DECLARATIONS_MIN,
                SOURCE_DECLARATIONS_MAX,
            ),
        ])
        .or_else(|| pattern_list_violation("source.include", &self.include))
        .or_else(|| pattern_list_violation("source.exclude", &self.exclude))
    }
}

fn default_source_files() -> u64 {
    SOURCE_FILES_DEFAULT
}

fn default_source_workspace_size() -> ByteSize {
    ByteSize::from_bytes(SOURCE_WORKSPACE_BYTES_DEFAULT)
}

fn default_source_declarations() -> u64 {
    SOURCE_DECLARATIONS_DEFAULT
}

/// The first pattern in `patterns` breaking [`PathPattern`]'s forward-slash-only contract,
/// patterns in list order.
fn pattern_list_violation(
    field: &'static str,
    patterns: &[PathPattern],
) -> Option<ConfigurationViolation> {
    patterns
        .iter()
        .find(|pattern| pattern.violation().is_some())
        .map(|pattern| ConfigurationViolation::PathPatternInvalid {
            field,
            pattern: pattern.0.clone(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::WorkspaceConfiguration;
    use serde_json::json;

    #[test]
    fn test_source_pattern_lists_accept_the_cap_and_refuse_above_it() {
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
    fn test_source_pattern_backslash_and_dot_segment_are_refused() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.source.include = vec![PathPattern("src\\lib.rs".to_owned())];
        let violation = configuration
            .validate()
            .expect_err("a backslash pattern must be refused");
        assert_eq!(
            violation,
            ConfigurationViolation::PathPatternInvalid {
                field: "source.include",
                pattern: "src\\lib.rs".to_owned(),
            }
        );

        let mut configuration = WorkspaceConfiguration::default();
        configuration.source.exclude = vec![PathPattern("../outside.rs".to_owned())];
        let violation = configuration
            .validate()
            .expect_err("a dot-segment pattern must be refused");
        assert_eq!(
            violation,
            ConfigurationViolation::PathPatternInvalid {
                field: "source.exclude",
                pattern: "../outside.rs".to_owned(),
            }
        );

        let mut configuration = WorkspaceConfiguration::default();
        configuration.source.include = vec![PathPattern("src/**/*.rs".to_owned())];
        assert_eq!(configuration.validate(), Ok(()));
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
        assert!(configuration.source.force_include.is_empty());
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_source_configuration_parses_force_include_globs() {
        let configuration: WorkspaceConfiguration = serde_json::from_value(json!({
            "source": {
                "force_include": [".plans/**"],
            }
        }))
        .expect("a force_include list must parse");
        assert_eq!(
            configuration.source.force_include,
            vec![PathPattern(".plans/**".to_owned())]
        );
        assert!(
            configuration.source.respect_gitignore,
            "reaching past one gitignored subtree does not turn the chain off"
        );
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_source_schema_advertises_the_force_include_bound() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let table = &schema["$defs"]["SourceConfiguration"]["properties"];
        assert_eq!(
            table["force_include"]["maxItems"], table["include"]["maxItems"],
            "the three glob lists carry one bound"
        );
        assert_eq!(table["force_include"]["default"], json!([]));
    }

    #[test]
    fn test_source_defaults_are_the_named_constants() {
        let table = SourceConfiguration::default();
        assert!(table.include.is_empty());
        assert!(table.exclude.is_empty());
        assert!(table.respect_gitignore);
        assert_eq!(table.files, SOURCE_FILES_DEFAULT);
        assert_eq!(
            table.workspace_size,
            ByteSize::from_bytes(SOURCE_WORKSPACE_BYTES_DEFAULT)
        );
        assert_eq!(table.declarations, SOURCE_DECLARATIONS_DEFAULT);
        let parsed: SourceConfiguration =
            serde_json::from_value(json!({})).expect("an empty table deserializes");
        assert_eq!(parsed, table);
        assert_eq!(table.violation(), None);
    }

    /// Sets one numeric key of the table to a value in its base unit.
    type Setter = fn(&mut SourceConfiguration, u64);

    #[test]
    fn test_source_numeric_bounds_are_enforced_naming_the_field() {
        let cases: [(&str, Setter, [u64; 2]); 3] = [
            (
                SOURCE_FILES_FIELD,
                |table, value| table.files = value,
                [SOURCE_FILES_MIN - 1, SOURCE_FILES_MAX + 1],
            ),
            (
                SOURCE_WORKSPACE_SIZE_FIELD,
                |table, value| table.workspace_size = ByteSize::from_bytes(value),
                [
                    SOURCE_WORKSPACE_BYTES_MIN - 1,
                    SOURCE_WORKSPACE_BYTES_MAX + 1,
                ],
            ),
            (
                SOURCE_DECLARATIONS_FIELD,
                |table, value| table.declarations = value,
                [SOURCE_DECLARATIONS_MIN - 1, SOURCE_DECLARATIONS_MAX + 1],
            ),
        ];
        for (field, set, values) in cases {
            for value in values {
                let mut configuration = WorkspaceConfiguration::default();
                set(&mut configuration.source, value);
                let violation = configuration
                    .validate()
                    .expect_err("a value outside its range must be refused");
                assert!(
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange { field: found, .. } if found == field
                    ),
                    "{field} = {value}: {violation:?}"
                );
            }
        }
    }

    #[test]
    fn test_source_bounds_accept_their_edges() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.source.files = SOURCE_FILES_MIN;
        configuration.source.workspace_size = ByteSize::from_bytes(SOURCE_WORKSPACE_BYTES_MAX);
        assert_eq!(configuration.validate(), Ok(()));
        configuration.source.declarations = SOURCE_DECLARATIONS_MIN;
        assert_eq!(configuration.validate(), Ok(()));
        configuration.source.files = SOURCE_FILES_MAX;
        configuration.source.workspace_size = ByteSize::from_bytes(SOURCE_WORKSPACE_BYTES_MIN);
        configuration.source.declarations = SOURCE_DECLARATIONS_MAX;
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_source_table_parses_every_key() {
        let table: SourceConfiguration = serde_json::from_value(json!({
            "include": ["src/**"],
            "exclude": ["**/generated/**"],
            "respect_gitignore": false,
            "files": 2000,
            "workspace_size": "1gb",
            "declarations": 250_000,
        }))
        .expect("every documented key parses");
        assert_eq!(table.files, 2000);
        assert_eq!(table.workspace_size, ByteSize::from_bytes(1 << 30));
        assert_eq!(table.declarations, 250_000);
        assert_eq!(table.violation(), None);
    }

    #[test]
    fn test_source_schema_defaults_and_ranges_equal_the_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let table = &schema["$defs"]["SourceConfiguration"]["properties"];
        let cases = [
            (
                "files default",
                &table["files"]["default"],
                json!(SOURCE_FILES_DEFAULT),
            ),
            (
                "files min",
                &table["files"]["minimum"],
                json!(SOURCE_FILES_MIN),
            ),
            (
                "files max",
                &table["files"]["maximum"],
                json!(SOURCE_FILES_MAX),
            ),
            (
                "workspace size default",
                &table["workspace_size"]["default"],
                json!(ByteSize::from_bytes(SOURCE_WORKSPACE_BYTES_DEFAULT)),
            ),
            (
                "workspace size range",
                &table["workspace_size"]["rift:range"],
                json!({
                    "min": ByteSize::from_bytes(SOURCE_WORKSPACE_BYTES_MIN),
                    "max": ByteSize::from_bytes(SOURCE_WORKSPACE_BYTES_MAX),
                }),
            ),
            (
                "declarations default",
                &table["declarations"]["default"],
                json!(SOURCE_DECLARATIONS_DEFAULT),
            ),
            (
                "declarations min",
                &table["declarations"]["minimum"],
                json!(SOURCE_DECLARATIONS_MIN),
            ),
            (
                "declarations max",
                &table["declarations"]["maximum"],
                json!(SOURCE_DECLARATIONS_MAX),
            ),
        ];
        for (name, found, expected) in cases {
            assert_eq!(*found, expected, "{name}");
        }
    }
}
