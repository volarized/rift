//! The `[dependencies]` table of `rift.toml`: whether the dependency index runs, how the
//! catalog is resolved, which cataloged packages are indexed, and the bounds the index
//! reads and holds under.

use crate::configuration::{
    ByteSize, CONFIGURATION_PATTERNS_MAX, ConfigurationViolation, Duration, first_out_of_range,
};
use crate::search::{PathPatternViolation, path_pattern_violation};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Whether the dependency index runs, by default.
pub const DEPENDENCIES_ENABLED_DEFAULT: bool = true;
/// How the catalog is resolved, by default: through each toolchain.
pub const DEPENDENCIES_RESOLUTION_DEFAULT: DependencyResolution = DependencyResolution::Auto;
/// Bytes one indexed package's selected source may hold, by default: 4 MiB.
pub const DEPENDENCIES_PACKAGE_BYTES_DEFAULT: u64 = 4 << 20;
/// Bytes one indexed package's selected source may hold, at least: 64 KiB.
pub const DEPENDENCIES_PACKAGE_BYTES_MIN: u64 = 64 << 10;
/// Bytes one indexed package's selected source may hold, at most: 1 GiB.
pub const DEPENDENCIES_PACKAGE_BYTES_MAX: u64 = 1 << 30;
/// Bytes every indexed package may hold together, by default: 256 MiB.
pub const DEPENDENCIES_INDEX_BYTES_DEFAULT: u64 = 256 << 20;
/// Bytes every indexed package may hold together, at least: 1 MiB.
pub const DEPENDENCIES_INDEX_BYTES_MIN: u64 = 1 << 20;
/// Bytes every indexed package may hold together, at most: 16 GiB.
pub const DEPENDENCIES_INDEX_BYTES_MAX: u64 = 16 << 30;
/// Files one indexed package's selection may hold, by default.
pub const DEPENDENCIES_PACKAGE_FILES_DEFAULT: u64 = 2_000;
/// Files one indexed package's selection may hold, at least.
pub const DEPENDENCIES_PACKAGE_FILES_MIN: u64 = 1;
/// Files one indexed package's selection may hold, at most.
pub const DEPENDENCIES_PACKAGE_FILES_MAX: u64 = 100_000;
/// Milliseconds one toolchain run may take before it is killed, by default: two
/// minutes.
pub const DEPENDENCIES_COMMAND_TIMEOUT_MS_DEFAULT: u64 = 120_000;
/// Milliseconds one toolchain run may take before it is killed, at least: one second.
pub const DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN: u64 = 1_000;
/// Milliseconds one toolchain run may take before it is killed, at most: one hour.
pub const DEPENDENCIES_COMMAND_TIMEOUT_MS_MAX: u64 = 3_600_000;
/// Bytes one package name pattern may hold, at most.
pub const PACKAGE_PATTERN_BYTES_MAX: usize = 256;

/// The spelling a [`PackageNamePattern`] must match: the form `PathPattern` advertises,
/// since a package name is matched the way a project path is.
const PACKAGE_PATTERN_REGEX: &str =
    r"^(?!/)(?!\.\.?(/|$))(?!.*(/\.\.?)(/|$))[^\\\u0000-\u001F\u007F]+$";

/// How the resolvers reach a package graph: through each toolchain, or from the
/// static inputs alone.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyResolution {
    /// Each resolver runs its toolchain and answers from its static inputs when the run
    /// fails.
    Auto,
    /// No toolchain runs. Each resolver answers from its static inputs, and an entry only
    /// a toolchain can name is reported as a degradation.
    Static,
}

/// One glob over a package's `<manager>/<name>`, such as `cargo/tokio`, `npm/@types/*`,
/// or `stdlib/*`. Forward-slash separated and at most 256 bytes; `*` never crosses `/`
/// and `**` does.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct PackageNamePattern(
    #[schemars(example = &"npm/@types/*")]
    #[schemars(length(min = 1, max = 256))]
    #[schemars(regex(pattern = PACKAGE_PATTERN_REGEX))]
    pub String,
);

impl PackageNamePattern {
    /// Classifies this pattern against the contract [`PackageNamePattern`] advertises.
    /// `schemars` constraints are declarative only, so acceptance calls this before the
    /// pattern reaches a glob engine.
    #[must_use]
    pub fn violation(&self) -> Option<PackagePatternViolation> {
        match self.0.as_bytes() {
            bytes if bytes.len() > PACKAGE_PATTERN_BYTES_MAX => {
                Some(PackagePatternViolation::TooLong)
            }
            _ => path_pattern_violation(&self.0).map(PackagePatternViolation::Form),
        }
    }
}

/// Reason a package name pattern breaks its contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackagePatternViolation {
    /// The pattern holds more than [`PACKAGE_PATTERN_BYTES_MAX`] bytes.
    TooLong,
    /// The pattern breaks the forward-slash-only form a project path pattern takes.
    Form(PathPatternViolation),
}

/// The `[dependencies]` table. The dependency index reads the packages the resolvers
/// catalog; this table turns the index off, selects how the catalog is resolved and
/// which cataloged packages are indexed, and bounds what the index reads and holds.
/// `include` and `exclude` are globs over `<manager>/<name>`: an empty `include` selects
/// every cataloged package, and an `exclude` match drops a package `include` selected.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_dependencies_ranges)]
pub struct DependenciesConfiguration {
    /// Whether the dependency index runs. `false` keeps the catalog resolved for
    /// `rift://map` and refuses a `scope` beyond `project` as `capability_unavailable`.
    #[serde(default = "default_dependencies_enabled")]
    pub enabled: bool,
    /// How the catalog is resolved: `auto` runs each toolchain, `static` reads the
    /// lockfiles and package caches alone.
    #[serde(default = "default_dependencies_resolution")]
    pub resolution: DependencyResolution,
    /// Globs over `<manager>/<name>` a cataloged package must match to be indexed.
    /// Empty selects every cataloged package.
    #[schemars(length(max = 64))]
    pub include: Vec<PackageNamePattern>,
    /// Globs over `<manager>/<name>` dropped from indexing. A match here is dropped
    /// even when `include` also matches it.
    #[schemars(length(max = 64))]
    pub exclude: Vec<PackageNamePattern>,
    /// Bytes one package's selected source may hold, 64kb to 1gb. A package past it is
    /// skipped.
    #[serde(default = "default_dependencies_package_size")]
    pub package_size: ByteSize,
    /// Bytes every indexed package may hold together, 1mb to 16gb. A package that
    /// would cross it is skipped.
    #[serde(default = "default_dependencies_index_size")]
    pub index_size: ByteSize,
    /// Files one package's selection may hold, 1 to 100000. A package past it is
    /// skipped.
    #[schemars(range(min = 1, max = 100_000))]
    #[serde(default = "default_dependencies_package_files")]
    pub package_files: u64,
    /// Wall-clock bound one toolchain run may take before it is killed, 1s to 1h.
    #[serde(default = "default_dependencies_command_timeout")]
    pub command_timeout: Duration,
}

impl Default for DependenciesConfiguration {
    fn default() -> Self {
        Self {
            enabled: default_dependencies_enabled(),
            resolution: default_dependencies_resolution(),
            include: Vec::new(),
            exclude: Vec::new(),
            package_size: default_dependencies_package_size(),
            index_size: default_dependencies_index_size(),
            package_files: default_dependencies_package_files(),
            command_timeout: default_dependencies_command_timeout(),
        }
    }
}

impl DependenciesConfiguration {
    /// The table's list-length and numeric bounds, then each pattern's contract, in key
    /// then list order.
    pub(crate) fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "dependencies.include",
                self.include.len() as u64,
                0,
                CONFIGURATION_PATTERNS_MAX as u64,
            ),
            (
                "dependencies.exclude",
                self.exclude.len() as u64,
                0,
                CONFIGURATION_PATTERNS_MAX as u64,
            ),
            (
                "dependencies.package_size",
                self.package_size.bytes(),
                DEPENDENCIES_PACKAGE_BYTES_MIN,
                DEPENDENCIES_PACKAGE_BYTES_MAX,
            ),
            (
                "dependencies.index_size",
                self.index_size.bytes(),
                DEPENDENCIES_INDEX_BYTES_MIN,
                DEPENDENCIES_INDEX_BYTES_MAX,
            ),
            (
                "dependencies.package_files",
                self.package_files,
                DEPENDENCIES_PACKAGE_FILES_MIN,
                DEPENDENCIES_PACKAGE_FILES_MAX,
            ),
            (
                "dependencies.command_timeout",
                self.command_timeout.milliseconds(),
                DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN,
                DEPENDENCIES_COMMAND_TIMEOUT_MS_MAX,
            ),
        ])
        .or_else(|| pattern_list_violation("dependencies.include", &self.include))
        .or_else(|| pattern_list_violation("dependencies.exclude", &self.exclude))
    }
}

fn default_dependencies_enabled() -> bool {
    DEPENDENCIES_ENABLED_DEFAULT
}

fn default_dependencies_resolution() -> DependencyResolution {
    DEPENDENCIES_RESOLUTION_DEFAULT
}

fn default_dependencies_package_size() -> ByteSize {
    ByteSize::from_bytes(DEPENDENCIES_PACKAGE_BYTES_DEFAULT)
}

fn default_dependencies_index_size() -> ByteSize {
    ByteSize::from_bytes(DEPENDENCIES_INDEX_BYTES_DEFAULT)
}

fn default_dependencies_package_files() -> u64 {
    DEPENDENCIES_PACKAGE_FILES_DEFAULT
}

fn default_dependencies_command_timeout() -> Duration {
    Duration::from_millis(DEPENDENCIES_COMMAND_TIMEOUT_MS_DEFAULT)
}

/// The first pattern in `patterns` breaking [`PackageNamePattern`]'s contract, patterns
/// in list order.
fn pattern_list_violation(
    field: &'static str,
    patterns: &[PackageNamePattern],
) -> Option<ConfigurationViolation> {
    patterns
        .iter()
        .find(|pattern| pattern.violation().is_some())
        .map(|pattern| ConfigurationViolation::PackagePatternInvalid {
            field,
            pattern: pattern.0.clone(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::WorkspaceConfiguration;
    use crate::read::PathPattern;
    use serde_json::json;

    fn pattern(value: &str) -> PackageNamePattern {
        PackageNamePattern(value.to_owned())
    }

    /// Sets one numeric key of the table to a value in its base unit.
    type Setter = fn(&mut DependenciesConfiguration, u64);

    #[test]
    fn test_dependencies_defaults_are_the_named_constants() {
        let table = DependenciesConfiguration::default();
        assert_eq!(table.enabled, DEPENDENCIES_ENABLED_DEFAULT);
        assert_eq!(table.resolution, DependencyResolution::Auto);
        assert!(table.include.is_empty());
        assert!(table.exclude.is_empty());
        assert_eq!(
            table.package_size,
            ByteSize::from_bytes(DEPENDENCIES_PACKAGE_BYTES_DEFAULT)
        );
        assert_eq!(
            table.index_size,
            ByteSize::from_bytes(DEPENDENCIES_INDEX_BYTES_DEFAULT)
        );
        assert_eq!(table.package_files, DEPENDENCIES_PACKAGE_FILES_DEFAULT);
        assert_eq!(
            table.command_timeout,
            Duration::from_millis(DEPENDENCIES_COMMAND_TIMEOUT_MS_DEFAULT)
        );
        let parsed: DependenciesConfiguration =
            serde_json::from_value(json!({})).expect("an empty table deserializes");
        assert_eq!(parsed, table);
        assert_eq!(table.violation(), None);
    }

    #[test]
    fn test_resolution_spells_auto_and_static_and_refuses_the_rest() {
        for (spelling, expected) in [
            ("auto", DependencyResolution::Auto),
            ("static", DependencyResolution::Static),
        ] {
            let table: DependenciesConfiguration =
                serde_json::from_value(json!({ "resolution": spelling }))
                    .expect("a documented spelling parses");
            assert_eq!(table.resolution, expected);
            assert_eq!(
                serde_json::to_value(expected).expect("serializes"),
                json!(spelling)
            );
        }
        let refused = serde_json::from_value::<DependenciesConfiguration>(
            json!({ "resolution": "toolchain" }),
        );
        assert!(refused.is_err(), "an undocumented spelling must be refused");
    }

    #[test]
    fn test_dependencies_numeric_bounds_are_enforced_naming_the_field() {
        let cases: [(&str, Setter, [u64; 2]); 4] = [
            (
                "dependencies.package_size",
                |table, value| table.package_size = ByteSize::from_bytes(value),
                [
                    DEPENDENCIES_PACKAGE_BYTES_MIN - 1,
                    DEPENDENCIES_PACKAGE_BYTES_MAX + 1,
                ],
            ),
            (
                "dependencies.index_size",
                |table, value| table.index_size = ByteSize::from_bytes(value),
                [
                    DEPENDENCIES_INDEX_BYTES_MIN - 1,
                    DEPENDENCIES_INDEX_BYTES_MAX + 1,
                ],
            ),
            (
                "dependencies.package_files",
                |table, value| table.package_files = value,
                [
                    DEPENDENCIES_PACKAGE_FILES_MIN - 1,
                    DEPENDENCIES_PACKAGE_FILES_MAX + 1,
                ],
            ),
            (
                "dependencies.command_timeout",
                |table, value| table.command_timeout = Duration::from_millis(value),
                [
                    DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN - 1,
                    DEPENDENCIES_COMMAND_TIMEOUT_MS_MAX + 1,
                ],
            ),
        ];
        for (field, set, values) in cases {
            for value in values {
                let mut configuration = WorkspaceConfiguration::default();
                set(&mut configuration.dependencies, value);
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
    fn test_dependencies_bounds_accept_their_edges() {
        let mut configuration = WorkspaceConfiguration::default();
        let table = &mut configuration.dependencies;
        table.package_size = ByteSize::from_bytes(DEPENDENCIES_PACKAGE_BYTES_MIN);
        table.index_size = ByteSize::from_bytes(DEPENDENCIES_INDEX_BYTES_MAX);
        table.package_files = DEPENDENCIES_PACKAGE_FILES_MAX;
        table.command_timeout = Duration::from_millis(DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN);
        table.include = vec![pattern("cargo/*"); CONFIGURATION_PATTERNS_MAX];
        table.exclude = vec![pattern("x".repeat(PACKAGE_PATTERN_BYTES_MAX).as_str())];
        assert_eq!(configuration.validate(), Ok(()));

        configuration
            .dependencies
            .include
            .push(pattern("one-too-many"));
        let violation = configuration
            .validate()
            .expect_err("an include list past the cap must be refused");
        assert!(matches!(
            violation,
            ConfigurationViolation::LimitOutOfRange {
                field: "dependencies.include",
                ..
            }
        ));
    }

    #[test]
    fn test_package_pattern_classifies_every_contract_rule() {
        let too_long = "x".repeat(PACKAGE_PATTERN_BYTES_MAX + 1);
        let cases = [
            (
                "",
                Some(PackagePatternViolation::Form(PathPatternViolation::Empty)),
            ),
            (too_long.as_str(), Some(PackagePatternViolation::TooLong)),
            (
                "/cargo/tokio",
                Some(PackagePatternViolation::Form(
                    PathPatternViolation::Absolute,
                )),
            ),
            (
                "cargo\\tokio",
                Some(PackagePatternViolation::Form(
                    PathPatternViolation::Backslash,
                )),
            ),
            (
                "cargo/to\u{0007}kio",
                Some(PackagePatternViolation::Form(
                    PathPatternViolation::ControlCharacter,
                )),
            ),
            (
                "cargo/../tokio",
                Some(PackagePatternViolation::Form(
                    PathPatternViolation::DotSegment,
                )),
            ),
            ("cargo/tokio", None),
            ("npm/@types/*", None),
            ("stdlib/*", None),
            ("**", None),
        ];
        for (value, expected) in cases {
            assert_eq!(pattern(value).violation(), expected, "{value:?}");
        }
    }

    #[test]
    fn test_pattern_refusals_name_the_list_and_the_pattern() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.dependencies.include = vec![pattern("cargo/*"), pattern("")];
        let violation = configuration
            .validate()
            .expect_err("an empty include pattern must be refused");
        assert_eq!(
            violation,
            ConfigurationViolation::PackagePatternInvalid {
                field: "dependencies.include",
                pattern: String::new(),
            }
        );

        let mut configuration = WorkspaceConfiguration::default();
        configuration.dependencies.exclude = vec![pattern("../tokio")];
        let violation = configuration
            .validate()
            .expect_err("a dot-segment exclude pattern must be refused");
        assert_eq!(
            violation,
            ConfigurationViolation::PackagePatternInvalid {
                field: "dependencies.exclude",
                pattern: "../tokio".to_owned(),
            }
        );
    }

    #[test]
    fn test_dependencies_table_parses_every_key_and_refuses_a_retired_one() {
        let table: DependenciesConfiguration = serde_json::from_value(json!({
            "enabled": false,
            "resolution": "static",
            "include": ["cargo/*", "stdlib/*"],
            "exclude": ["cargo/helper"],
            "package_size": "8mb",
            "index_size": "1gb",
            "package_files": 10,
            "command_timeout": "5m",
        }))
        .expect("every documented key parses");
        assert!(!table.enabled);
        assert_eq!(table.resolution, DependencyResolution::Static);
        assert_eq!(table.include, [pattern("cargo/*"), pattern("stdlib/*")]);
        assert_eq!(table.exclude, [pattern("cargo/helper")]);
        assert_eq!(table.package_size, ByteSize::from_bytes(8 << 20));
        assert_eq!(table.index_size, ByteSize::from_bytes(1 << 30));
        assert_eq!(table.package_files, 10);
        assert_eq!(table.command_timeout, Duration::from_millis(300_000));
        assert_eq!(table.violation(), None);
        let refused =
            serde_json::from_value::<DependenciesConfiguration>(json!({ "package_bytes_max": 1 }));
        assert!(refused.is_err(), "a retired key must be refused");
    }

    #[test]
    fn test_dependencies_schema_defaults_and_ranges_equal_the_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let table = &schema["$defs"]["DependenciesConfiguration"]["properties"];
        let cases = [
            ("enabled default", &table["enabled"]["default"], json!(true)),
            (
                "resolution default",
                &table["resolution"]["default"],
                json!("auto"),
            ),
            ("include default", &table["include"]["default"], json!([])),
            (
                "include max",
                &table["include"]["maxItems"],
                json!(CONFIGURATION_PATTERNS_MAX),
            ),
            (
                "exclude max",
                &table["exclude"]["maxItems"],
                json!(CONFIGURATION_PATTERNS_MAX),
            ),
            (
                "package size default",
                &table["package_size"]["default"],
                json!(ByteSize::from_bytes(DEPENDENCIES_PACKAGE_BYTES_DEFAULT)),
            ),
            (
                "package size range",
                &table["package_size"]["rift:range"],
                json!({
                    "min": ByteSize::from_bytes(DEPENDENCIES_PACKAGE_BYTES_MIN),
                    "max": ByteSize::from_bytes(DEPENDENCIES_PACKAGE_BYTES_MAX),
                }),
            ),
            (
                "index size default",
                &table["index_size"]["default"],
                json!(ByteSize::from_bytes(DEPENDENCIES_INDEX_BYTES_DEFAULT)),
            ),
            (
                "index size range",
                &table["index_size"]["rift:range"],
                json!({
                    "min": ByteSize::from_bytes(DEPENDENCIES_INDEX_BYTES_MIN),
                    "max": ByteSize::from_bytes(DEPENDENCIES_INDEX_BYTES_MAX),
                }),
            ),
            (
                "package files default",
                &table["package_files"]["default"],
                json!(DEPENDENCIES_PACKAGE_FILES_DEFAULT),
            ),
            (
                "package files min",
                &table["package_files"]["minimum"],
                json!(DEPENDENCIES_PACKAGE_FILES_MIN),
            ),
            (
                "package files max",
                &table["package_files"]["maximum"],
                json!(DEPENDENCIES_PACKAGE_FILES_MAX),
            ),
            (
                "command timeout default",
                &table["command_timeout"]["default"],
                json!(Duration::from_millis(
                    DEPENDENCIES_COMMAND_TIMEOUT_MS_DEFAULT
                )),
            ),
            (
                "command timeout range",
                &table["command_timeout"]["rift:range"],
                json!({
                    "min": Duration::from_millis(DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN),
                    "max": Duration::from_millis(DEPENDENCIES_COMMAND_TIMEOUT_MS_MAX),
                }),
            ),
        ];
        for (name, found, expected) in cases {
            assert_eq!(*found, expected, "{name}");
        }
        let resolution = &schema["$defs"]["DependencyResolution"];
        assert_eq!(
            resolution["oneOf"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|arm| arm["const"].clone())
                .collect::<Vec<_>>(),
            [json!("auto"), json!("static")],
            "{resolution}"
        );
    }

    #[test]
    fn test_package_pattern_schema_states_the_path_pattern_form_and_its_length() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let package = &schema["$defs"]["PackageNamePattern"];
        let path = serde_json::to_value(schemars::schema_for!(PathPattern)).expect("schema");
        assert_eq!(package["pattern"], path["pattern"]);
        assert_eq!(package["pattern"], json!(PACKAGE_PATTERN_REGEX));
        assert_eq!(package["minLength"], json!(1));
        assert_eq!(package["maxLength"], json!(PACKAGE_PATTERN_BYTES_MAX));
        assert_eq!(package["examples"], json!(["npm/@types/*"]));
    }
}
