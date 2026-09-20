//! The `[dependencies]` table of `rift.toml`, and the static dependency context models.
//!
//! The table states how the catalog is resolved, which packages the operator names
//! beside the ones the workspace's files state, and the bounds the package index reads
//! and holds under. [`PackageContextEntry`] carries one package the workspace depends
//! on, as an exact version a lockfile pins or as the requirement a manifest declares.

use crate::configuration::{ByteSize, ConfigurationViolation, Duration, first_out_of_range};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
/// Entries the configured package list may hold, at most.
pub const DEPENDENCIES_PACKAGES_MAX: usize = 20_000;

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

/// Whether a global package index can answer for one package.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PackageAvailability {
    /// The entry names a package a public registry serves: crates.io, npm, or the
    /// Python Package Index.
    Canonical,
    /// The entry names a path, git, or custom-registry package only this machine can
    /// answer for.
    LocalOnly,
}

/// One package the workspace depends on, as its manifests and lockfiles state it.
///
/// Entries order by manager, then name, then the selector they state: a declared
/// requirement before an exact version.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = crate::schema::require_one_package_context_selector)]
pub struct PackageContextEntry {
    /// Package manager or ecosystem name.
    #[schemars(length(max = 128))]
    pub manager: String,
    /// Package name in that ecosystem.
    #[schemars(length(max = 4096))]
    pub name: String,
    /// The exact version a lockfile pins. Absent when only a manifest names the package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub version: Option<String>,
    /// The version requirement a manifest declares. Absent when a lockfile pins the
    /// version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub requirement: Option<String>,
    /// Whether a global package index can answer for this entry.
    pub availability: PackageAvailability,
}

impl PackageContextEntry {
    /// One entry stating exactly one selector, so the rule the schema advertises holds
    /// by construction.
    #[must_use]
    pub fn new(
        manager: &str,
        name: &str,
        selector: PackageSelector,
        availability: PackageAvailability,
    ) -> Self {
        let (version, requirement) = match selector {
            PackageSelector::Version(version) => (Some(version), None),
            PackageSelector::Requirement(requirement) => (None, Some(requirement)),
        };
        Self {
            manager: manager.to_owned(),
            name: name.to_owned(),
            version,
            requirement,
            availability,
        }
    }

    /// Classifies this entry against the exactly-one selector rule the schema
    /// advertises. `schemars` constraints are declarative only, so a deserialized entry
    /// is classified before it reaches a reader.
    #[must_use]
    pub fn violation(&self) -> Option<PackageSelectorViolation> {
        selector_violation(self.version.as_deref(), self.requirement.as_deref())
    }
}

/// What one package entry states about a package's version.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PackageSelector {
    /// The exact version a lockfile pins.
    Version(String),
    /// The version requirement a manifest declares.
    Requirement(String),
}

/// One package the `[dependencies]` `packages` list names.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = crate::schema::require_one_configured_package_selector)]
pub struct ConfiguredPackage {
    /// Package manager or ecosystem name, as `cargo`, `npm`, or `pypi`.
    #[schemars(length(max = 128))]
    pub manager: String,
    /// Package name in that ecosystem.
    #[schemars(length(max = 4096))]
    pub name: String,
    /// The exact version this entry pins. Set this or `requirement`, never both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub version: Option<String>,
    /// The version requirement this entry declares. Set this or `version`, never both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub requirement: Option<String>,
}

impl ConfiguredPackage {
    /// Classifies this entry against the exactly-one selector rule the schema
    /// advertises. Acceptance calls this before the entry reaches the context.
    #[must_use]
    pub fn violation(&self) -> Option<PackageSelectorViolation> {
        selector_violation(self.version.as_deref(), self.requirement.as_deref())
    }

    /// The one selector this entry states. Absent when it states both or neither, the
    /// case acceptance refuses.
    #[must_use]
    pub fn selector(&self) -> Option<PackageSelector> {
        match (&self.version, &self.requirement) {
            (Some(version), None) => Some(PackageSelector::Version(version.clone())),
            (None, Some(requirement)) => Some(PackageSelector::Requirement(requirement.clone())),
            _ => None,
        }
    }
}

/// Reason one package entry states no single version selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageSelectorViolation {
    /// The entry names `version` and `requirement` together.
    SelectorConflict,
    /// The entry names neither `version` nor `requirement`.
    SelectorMissing,
}

/// The exactly-one rule both package entry models carry: a lockfile pins a `version`, a
/// manifest declares a `requirement`, and no entry states both or neither.
fn selector_violation(
    version: Option<&str>,
    requirement: Option<&str>,
) -> Option<PackageSelectorViolation> {
    match (version, requirement) {
        (Some(_), Some(_)) => Some(PackageSelectorViolation::SelectorConflict),
        (None, None) => Some(PackageSelectorViolation::SelectorMissing),
        _ => None,
    }
}

/// The `[dependencies]` table. It states how the catalog is resolved, which packages
/// the operator names beside the ones the workspace's manifests and lockfiles state,
/// and the bounds the package index reads and holds under.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_dependencies_ranges)]
pub struct DependenciesConfiguration {
    /// How the catalog is resolved: `auto` runs each toolchain, `static` reads the
    /// lockfiles and package caches alone.
    #[serde(default = "default_dependencies_resolution")]
    pub resolution: DependencyResolution,
    /// Packages carried beside the ones the workspace's manifests and lockfiles state.
    /// Each entry names exactly one of `version` and `requirement`.
    #[schemars(length(max = 20_000))]
    pub packages: Vec<ConfiguredPackage>,
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
            resolution: default_dependencies_resolution(),
            packages: Vec::new(),
            package_size: default_dependencies_package_size(),
            index_size: default_dependencies_index_size(),
            package_files: default_dependencies_package_files(),
            command_timeout: default_dependencies_command_timeout(),
        }
    }
}

impl DependenciesConfiguration {
    /// The table's list-length and numeric bounds, then each configured package's
    /// selector, in key then list order.
    pub(crate) fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "dependencies.packages",
                self.packages.len() as u64,
                0,
                DEPENDENCIES_PACKAGES_MAX as u64,
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
        .or_else(|| configured_package_violation(&self.packages))
    }
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

/// The first configured package stating no single version selector, entries in list
/// order.
fn configured_package_violation(packages: &[ConfiguredPackage]) -> Option<ConfigurationViolation> {
    packages
        .iter()
        .find(|package| package.violation().is_some())
        .map(|package| ConfigurationViolation::PackageSelectorInvalid {
            field: "dependencies.packages",
            package: format!("{}/{}", package.manager, package.name),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::WorkspaceConfiguration;
    use serde_json::json;

    /// Sets one numeric key of the table to a value in its base unit.
    type Setter = fn(&mut DependenciesConfiguration, u64);

    fn configured(
        manager: &str,
        name: &str,
        version: Option<&str>,
        requirement: Option<&str>,
    ) -> ConfiguredPackage {
        ConfiguredPackage {
            manager: manager.to_owned(),
            name: name.to_owned(),
            version: version.map(str::to_owned),
            requirement: requirement.map(str::to_owned),
        }
    }

    fn context_entry(version: Option<&str>, requirement: Option<&str>) -> PackageContextEntry {
        PackageContextEntry {
            manager: "cargo".to_owned(),
            name: "serde".to_owned(),
            version: version.map(str::to_owned),
            requirement: requirement.map(str::to_owned),
            availability: PackageAvailability::Canonical,
        }
    }

    #[test]
    fn test_dependencies_defaults_are_the_named_constants() {
        let table = DependenciesConfiguration::default();
        assert_eq!(table.resolution, DependencyResolution::Auto);
        assert!(table.packages.is_empty());
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
        table.packages =
            vec![configured("cargo", "serde", Some("1.0.228"), None); DEPENDENCIES_PACKAGES_MAX];
        assert_eq!(configuration.validate(), Ok(()));

        configuration
            .dependencies
            .packages
            .push(configured("cargo", "tokio", None, Some("^1")));
        let violation = configuration
            .validate()
            .expect_err("a package list past the cap must be refused");
        assert!(matches!(
            violation,
            ConfigurationViolation::LimitOutOfRange {
                field: "dependencies.packages",
                ..
            }
        ));
    }

    #[test]
    fn test_package_selector_classifies_both_and_neither() {
        assert_eq!(context_entry(Some("1.0.228"), None).violation(), None);
        assert_eq!(context_entry(None, Some("^1.0")).violation(), None);
        assert_eq!(
            context_entry(Some("1.0.228"), Some("^1.0")).violation(),
            Some(PackageSelectorViolation::SelectorConflict)
        );
        assert_eq!(
            context_entry(None, None).violation(),
            Some(PackageSelectorViolation::SelectorMissing)
        );
        assert_eq!(
            configured("npm", "left-pad", Some("1.3.0"), Some("^1")).violation(),
            Some(PackageSelectorViolation::SelectorConflict)
        );
        assert_eq!(
            configured("npm", "left-pad", None, None).violation(),
            Some(PackageSelectorViolation::SelectorMissing)
        );
    }

    #[test]
    fn test_one_selector_builds_an_entry_and_reads_back_from_a_configured_package() {
        let pinned = PackageContextEntry::new(
            "cargo",
            "serde",
            PackageSelector::Version("1.0.228".to_owned()),
            PackageAvailability::Canonical,
        );
        assert_eq!(pinned, context_entry(Some("1.0.228"), None));
        let declared = PackageContextEntry::new(
            "cargo",
            "serde",
            PackageSelector::Requirement("^1.0".to_owned()),
            PackageAvailability::Canonical,
        );
        assert_eq!(declared, context_entry(None, Some("^1.0")));

        assert_eq!(
            configured("cargo", "serde", Some("1.0.228"), None).selector(),
            Some(PackageSelector::Version("1.0.228".to_owned()))
        );
        assert_eq!(
            configured("cargo", "serde", None, Some("^1.0")).selector(),
            Some(PackageSelector::Requirement("^1.0".to_owned()))
        );
        assert_eq!(
            configured("cargo", "serde", Some("1.0.228"), Some("^1.0")).selector(),
            None
        );
        assert_eq!(configured("cargo", "serde", None, None).selector(), None);
    }

    #[test]
    fn test_a_configured_package_naming_both_or_neither_selector_is_refused() {
        for package in [
            configured("cargo", "serde", Some("1.0.228"), Some("^1.0")),
            configured("cargo", "serde", None, None),
        ] {
            let mut configuration = WorkspaceConfiguration::default();
            configuration.dependencies.packages =
                vec![configured("cargo", "tokio", Some("1.53.1"), None), package];
            let violation = configuration
                .validate()
                .expect_err("an entry stating no single selector must be refused");
            assert_eq!(
                violation,
                ConfigurationViolation::PackageSelectorInvalid {
                    field: "dependencies.packages",
                    package: "cargo/serde".to_owned(),
                }
            );
        }
    }

    #[test]
    fn test_dependencies_table_parses_every_key_and_refuses_a_removed_one() {
        let table: DependenciesConfiguration = serde_json::from_value(json!({
            "resolution": "static",
            "packages": [
                { "manager": "cargo", "name": "serde", "version": "1.0.228" },
                { "manager": "npm", "name": "typescript", "requirement": "^5.9.0" },
            ],
            "package_size": "8mb",
            "index_size": "1gb",
            "package_files": 10,
            "command_timeout": "5m",
        }))
        .expect("every documented key parses");
        assert_eq!(table.resolution, DependencyResolution::Static);
        assert_eq!(
            table.packages,
            [
                configured("cargo", "serde", Some("1.0.228"), None),
                configured("npm", "typescript", None, Some("^5.9.0")),
            ]
        );
        assert_eq!(table.package_size, ByteSize::from_bytes(8 << 20));
        assert_eq!(table.index_size, ByteSize::from_bytes(1 << 30));
        assert_eq!(table.package_files, 10);
        assert_eq!(table.command_timeout, Duration::from_millis(300_000));
        assert_eq!(table.violation(), None);
        for removed in ["enabled", "include", "exclude", "package_bytes_max"] {
            let refused = serde_json::from_value::<DependenciesConfiguration>(
                json!({ removed: serde_json::Value::Null }),
            );
            assert!(refused.is_err(), "a removed key must be refused: {removed}");
        }
    }

    #[test]
    fn test_dependencies_schema_defaults_and_ranges_equal_the_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let table = &schema["$defs"]["DependenciesConfiguration"]["properties"];
        let cases = [
            (
                "resolution default",
                &table["resolution"]["default"],
                json!("auto"),
            ),
            ("packages default", &table["packages"]["default"], json!([])),
            (
                "packages max",
                &table["packages"]["maxItems"],
                json!(DEPENDENCIES_PACKAGES_MAX),
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

    /// The schema advertises the exactly-one selector rule both entry models enforce at
    /// runtime, so a validating reader and the server refuse the same documents.
    #[test]
    fn test_selector_schema_admits_exactly_what_the_classifier_accepts() {
        let cases = [
            (
                "PackageContextEntry",
                serde_json::to_value(schemars::schema_for!(PackageContextEntry)).expect("schema"),
                json!({ "manager": "cargo", "name": "serde", "availability": "canonical" }),
            ),
            (
                "ConfiguredPackage",
                serde_json::to_value(schemars::schema_for!(ConfiguredPackage)).expect("schema"),
                json!({ "manager": "cargo", "name": "serde" }),
            ),
        ];
        for (model, schema, base) in cases {
            let validator = jsonschema::validator_for(&schema).expect("a selector schema compiles");
            let with = |selectors: serde_json::Value| {
                let mut document = base.clone();
                let object = document.as_object_mut().expect("an entry is an object");
                for (key, value) in selectors.as_object().expect("selectors are an object") {
                    object.insert(key.clone(), value.clone());
                }
                document
            };
            assert!(
                validator.is_valid(&with(json!({ "version": "1.0.228" }))),
                "{model} must admit an exact version"
            );
            assert!(
                validator.is_valid(&with(json!({ "requirement": "^1.0" }))),
                "{model} must admit a declared requirement"
            );
            assert!(
                !validator.is_valid(&with(
                    json!({ "version": "1.0.228", "requirement": "^1.0" })
                )),
                "{model} must refuse both selectors"
            );
            assert!(
                !validator.is_valid(&base),
                "{model} must refuse an entry stating neither selector"
            );
        }
    }

    /// Two entries sort by manager, then name, then the selector they state.
    #[test]
    fn test_context_entries_sort_by_manager_name_then_selector() {
        let mut entries = [
            PackageContextEntry {
                manager: "npm".to_owned(),
                name: "typescript".to_owned(),
                version: None,
                requirement: Some("^5.9.0".to_owned()),
                availability: PackageAvailability::Canonical,
            },
            context_entry(None, Some("^1.0")),
            context_entry(Some("1.0.228"), None),
        ];
        entries.sort();
        let spelled: Vec<String> = entries
            .iter()
            .map(|entry| {
                let selector = entry
                    .version
                    .as_deref()
                    .or(entry.requirement.as_deref())
                    .unwrap_or_default();
                format!("{}/{}@{selector}", entry.manager, entry.name)
            })
            .collect();
        assert_eq!(
            spelled,
            [
                "cargo/serde@^1.0",
                "cargo/serde@1.0.228",
                "npm/typescript@^5.9.0"
            ]
        );
    }
}
