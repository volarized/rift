//! The `[dependencies]` table of `rift.toml`, and the static dependency context models.
//!
//! The table states whether the dependency context runs the standard library version
//! probes, how long one probe may take, and which packages the operator names beside the
//! ones the workspace's files state. [`PackageContextEntry`] carries one package the
//! workspace depends on, as an exact version a lockfile pins or as the requirement a
//! manifest declares. [`RequestedPackage`] carries one package a read names for itself.

use crate::configuration::{ConfigurationViolation, Duration, first_out_of_range};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Whether the dependency context runs the version probes, by default: it does.
pub const DEPENDENCIES_RESOLUTION_DEFAULT: DependencyResolution = DependencyResolution::Auto;
/// Milliseconds one version probe may take before it is killed, by default: two
/// minutes.
pub const DEPENDENCIES_COMMAND_TIMEOUT_MS_DEFAULT: u64 = 120_000;
/// Milliseconds one version probe may take before it is killed, at least: one second.
pub const DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN: u64 = 1_000;
/// Milliseconds one version probe may take before it is killed, at most: one hour.
pub const DEPENDENCIES_COMMAND_TIMEOUT_MS_MAX: u64 = 3_600_000;
mod collection;
pub use collection::{
    DEPENDENCIES_COLLECTION_BYTES_MAX, DEPENDENCIES_COLLECTION_COUNT_MAX,
    DEPENDENCIES_COLLECTION_MANIFESTS_MAX, DEPENDENCIES_COLLECTION_NESTING_MAX,
    DEPENDENCIES_COLLECTION_OUTPUT_MAX, DependenciesCollectionConfiguration,
};

/// Entries the configured package list may hold, at most.
pub const DEPENDENCIES_PACKAGES_MAX: usize = 20_000;
/// Entries one read's `packages` argument may hold, at most.
pub const REQUESTED_PACKAGES_MAX: usize = 64;
/// Characters a requested package's `manager` holds, at most.
pub const PACKAGE_MANAGER_CHARS_MAX: usize = 128;
/// Characters a requested package's `name` holds, at most.
pub const PACKAGE_NAME_CHARS_MAX: usize = 4_096;
/// Characters a requested package's `version` holds, at most.
pub const PACKAGE_VERSION_CHARS_MAX: usize = 4_096;
/// The requirement every stable release admits under Cargo's, npm's, and PEP 440's
/// version rules, so the global index answers it from its newest collected release.
pub const REQUIREMENT_ANY: &str = ">=0";

/// Whether the dependency context runs the standard library version probes, or reads
/// the static inputs alone.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyResolution {
    /// A standard library version no pin names is read from `rustc --version` or
    /// `node --version`, and the static reading stands when the probe fails.
    Auto,
    /// No program runs. Every standard library version comes from the pins and project
    /// files alone.
    Static,
}

/// Whether a global package index can answer for one package, and where the entry's
/// source comes from when none can.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PackageAvailability {
    /// The entry names a package a public registry serves, crates.io, npm, or the
    /// Python Package Index, or a standard library: `stdlib/rust`, `stdlib/node`, or
    /// `stdlib/python`.
    Canonical,
    /// The entry names a path outside the workspace, or an archive at a path, that only
    /// this machine can read.
    Path,
    /// The entry names a git repository.
    Git,
    /// The entry names a registry other than the public one its package manager reads.
    PrivateRegistry,
    /// The entry names a package fetched from a URL no registry serves, such as a uv
    /// wheel URL or an npm tarball URL.
    Url,
}

/// The `package_unavailable` reason for a [`PackageAvailability::Path`] entry.
pub const PATH_UNAVAILABLE_REASON: &str = "Currently rift doesn't support indexing dependencies \
    by path. If you're interested in this capability, please upvote it at \
    https://github.com/volarized/rift/issues/392.";
/// The `package_unavailable` reason for a [`PackageAvailability::Git`] entry.
pub const GIT_UNAVAILABLE_REASON: &str = "Currently rift doesn't support indexing dependencies \
    from git repositories. If you're interested in this capability, please upvote it at \
    https://github.com/volarized/rift/issues/393.";
/// The `package_unavailable` reason for a [`PackageAvailability::PrivateRegistry`] entry.
pub const PRIVATE_REGISTRY_UNAVAILABLE_REASON: &str =
    "Currently rift doesn't support indexing dependencies from private registries.";
/// The `package_unavailable` reason for a [`PackageAvailability::Url`] entry.
pub const URL_UNAVAILABLE_REASON: &str =
    "Currently rift doesn't support indexing dependencies from URLs.";

impl PackageAvailability {
    /// Why no global package index answers for an entry of this kind, naming the
    /// capability Rift does not have yet. Absent for a [`Self::Canonical`] entry, which
    /// the global index answers for.
    #[must_use]
    pub const fn unavailable_reason(self) -> Option<&'static str> {
        match self {
            Self::Canonical => None,
            Self::Path => Some(PATH_UNAVAILABLE_REASON),
            Self::Git => Some(GIT_UNAVAILABLE_REASON),
            Self::PrivateRegistry => Some(PRIVATE_REGISTRY_UNAVAILABLE_REASON),
            Self::Url => Some(URL_UNAVAILABLE_REASON),
        }
    }
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

/// One package a `get_symbol` or `search` request names beside the dependency context.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [
    {
        "manager": "cargo",
        "name": "tokio",
        "version": "1.47.1"
    },
    {
        "manager": "npm",
        "name": "zod"
    }
]))]
pub struct RequestedPackage {
    /// Package manager or ecosystem name, as `cargo`, `npm`, or `pypi`.
    #[schemars(length(min = 1, max = 128))]
    pub manager: String,
    /// Package name in that ecosystem.
    #[schemars(length(min = 1, max = 4096))]
    pub name: String,
    /// The exact version to read. Absent, the entry asks for the requirement `>=0`, which
    /// the global index answers from its newest collected release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub version: Option<String>,
}

impl RequestedPackage {
    /// Classifies this entry against the lengths the schema advertises, in field order.
    /// `schemars` constraints are declarative only, so the server classifies each entry
    /// before it reaches the global API.
    #[must_use]
    pub fn violation(&self) -> Option<RequestedPackageViolation> {
        let fields = [
            (
                Some(self.manager.as_str()),
                PACKAGE_MANAGER_CHARS_MAX,
                RequestedPackageViolation::ManagerLength,
            ),
            (
                Some(self.name.as_str()),
                PACKAGE_NAME_CHARS_MAX,
                RequestedPackageViolation::NameLength,
            ),
            (
                self.version.as_deref(),
                PACKAGE_VERSION_CHARS_MAX,
                RequestedPackageViolation::VersionLength,
            ),
        ];
        fields
            .into_iter()
            .find_map(|(value, chars_max, violation)| {
                let within_length =
                    value.is_none_or(|value| (1..=chars_max).contains(&value.chars().count()));
                (!within_length).then_some(violation)
            })
    }

    /// The context entry this package goes out as: its exact `version`, or the
    /// requirement [`REQUIREMENT_ANY`] when it names none.
    ///
    /// The entry is `canonical`: the request names a package by manager and name, which
    /// is what a public registry answers for, and carries no source this machine could
    /// classify otherwise.
    #[must_use]
    pub fn context_entry(&self) -> PackageContextEntry {
        let selector = self.version.as_ref().map_or_else(
            || PackageSelector::Requirement(REQUIREMENT_ANY.to_owned()),
            |version| PackageSelector::Version(version.clone()),
        );
        PackageContextEntry::new(
            &self.manager,
            &self.name,
            selector,
            PackageAvailability::Canonical,
        )
    }
}

/// Rule one requested package breaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, strum::AsRefStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RequestedPackageViolation {
    /// `manager` is empty, or longer than [`PACKAGE_MANAGER_CHARS_MAX`] characters.
    ManagerLength,
    /// `name` is empty, or longer than [`PACKAGE_NAME_CHARS_MAX`] characters.
    NameLength,
    /// `version` is empty, or longer than [`PACKAGE_VERSION_CHARS_MAX`] characters.
    VersionLength,
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

/// The `[dependencies]` table. It states whether the dependency context runs the
/// standard library version probes, how long one probe may take, and which packages the
/// operator names beside the ones the workspace's manifests and lockfiles state.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_dependencies_ranges)]
pub struct DependenciesConfiguration {
    /// File, directory, manifest, package, and version probe collection bounds.
    pub collection: DependenciesCollectionConfiguration,
    /// Whether a standard library version probe runs: `auto` runs `rustc --version` or
    /// `node --version` when no pin names the version, `static` reads the pins and project
    /// files alone and runs no program.
    #[serde(default = "default_dependencies_resolution")]
    pub resolution: DependencyResolution,
    /// Packages carried beside the ones the workspace's manifests and lockfiles state.
    /// Each entry names exactly one of `version` and `requirement`.
    #[schemars(length(max = 20_000))]
    pub packages: Vec<ConfiguredPackage>,
    /// Wall-clock bound one standard library version probe may take before it is
    /// killed, 1s to 1h.
    #[serde(default = "default_dependencies_command_timeout")]
    pub command_timeout: Duration,
}

impl Default for DependenciesConfiguration {
    fn default() -> Self {
        Self {
            resolution: default_dependencies_resolution(),
            collection: DependenciesCollectionConfiguration::default(),
            packages: Vec::new(),
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
                "dependencies.command_timeout",
                self.command_timeout.milliseconds(),
                DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN,
                DEPENDENCIES_COMMAND_TIMEOUT_MS_MAX,
            ),
        ])
        .or_else(|| configured_package_violation(&self.packages))
        .or_else(|| self.collection.violation())
    }
}

fn default_dependencies_resolution() -> DependencyResolution {
    DEPENDENCIES_RESOLUTION_DEFAULT
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
        let cases: [(&str, Setter, [u64; 2]); 1] = [(
            "dependencies.command_timeout",
            |table, value| table.command_timeout = Duration::from_millis(value),
            [
                DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN - 1,
                DEPENDENCIES_COMMAND_TIMEOUT_MS_MAX + 1,
            ],
        )];
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
        assert_eq!(table.command_timeout, Duration::from_millis(300_000));
        assert_eq!(table.violation(), None);
        for removed in [
            "enabled",
            "include",
            "exclude",
            "package_bytes_max",
            "package_size",
            "index_size",
            "package_files",
        ] {
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

    /// Each kind no public registry serves names the missing capability, linking the
    /// enhancement issue that collects demand for path and git dependencies alone.
    #[test]
    fn test_each_unserved_kind_names_its_missing_capability() {
        assert_eq!(PackageAvailability::Canonical.unavailable_reason(), None);
        let cases = [
            (
                PackageAvailability::Path,
                "path",
                Some("https://github.com/volarized/rift/issues/392."),
            ),
            (
                PackageAvailability::Git,
                "git",
                Some("https://github.com/volarized/rift/issues/393."),
            ),
            (
                PackageAvailability::PrivateRegistry,
                "private_registry",
                None,
            ),
            (PackageAvailability::Url, "url", None),
        ];
        for (availability, spelling, link) in cases {
            let reason = availability
                .unavailable_reason()
                .unwrap_or_else(|| panic!("{spelling} names no reason"));
            assert!(
                reason.starts_with("Currently rift doesn't support indexing dependencies "),
                "{reason}"
            );
            assert_eq!(
                link.is_some_and(|link| reason.ends_with(link)),
                link.is_some(),
                "{reason}"
            );
            assert!(
                !reason.contains("  ") && !reason.contains('\n'),
                "{reason:?}"
            );
            assert_eq!(
                serde_json::to_value(availability).expect("serializes"),
                json!(spelling)
            );
        }
        assert!(
            serde_json::from_value::<PackageAvailability>(json!("local_only")).is_err(),
            "the kinds replace `local_only`"
        );
    }

    fn requested(manager: &str, name: &str, version: Option<&str>) -> RequestedPackage {
        RequestedPackage {
            manager: manager.to_owned(),
            name: name.to_owned(),
            version: version.map(str::to_owned),
        }
    }

    /// Each field is classified against its advertised length in field order: empty and
    /// one character past the bound refuse, the bound itself and an absent version pass.
    #[test]
    fn test_a_requested_package_is_classified_by_each_field_length() {
        let at = |chars_max: usize| "a".repeat(chars_max);
        let past = |chars_max: usize| "a".repeat(chars_max + 1);
        let cases = [
            (requested("cargo", "serde", None), None),
            (requested("cargo", "serde", Some("1.0.228")), None),
            (
                requested(
                    &at(PACKAGE_MANAGER_CHARS_MAX),
                    &at(PACKAGE_NAME_CHARS_MAX),
                    Some(&at(PACKAGE_VERSION_CHARS_MAX)),
                ),
                None,
            ),
            (
                requested("", "serde", None),
                Some(RequestedPackageViolation::ManagerLength),
            ),
            (
                requested(&past(PACKAGE_MANAGER_CHARS_MAX), "serde", None),
                Some(RequestedPackageViolation::ManagerLength),
            ),
            (
                requested("cargo", "", Some("")),
                Some(RequestedPackageViolation::NameLength),
            ),
            (
                requested("cargo", &past(PACKAGE_NAME_CHARS_MAX), None),
                Some(RequestedPackageViolation::NameLength),
            ),
            (
                requested("cargo", "serde", Some("")),
                Some(RequestedPackageViolation::VersionLength),
            ),
            (
                requested("cargo", "serde", Some(&past(PACKAGE_VERSION_CHARS_MAX))),
                Some(RequestedPackageViolation::VersionLength),
            ),
        ];
        for (package, expected) in cases {
            assert_eq!(package.violation(), expected, "{package:?}");
        }
    }

    /// A length counts characters, as the schema's `maxLength` does, so a multi-byte name
    /// at the character bound passes.
    #[test]
    fn test_a_requested_package_length_counts_characters() {
        let name = "é".repeat(PACKAGE_NAME_CHARS_MAX);
        assert!(name.len() > PACKAGE_NAME_CHARS_MAX);
        assert_eq!(requested("npm", &name, None).violation(), None);
    }

    /// A requested package goes out canonical, at its exact version or as `>=0`.
    #[test]
    fn test_a_requested_package_goes_out_as_one_canonical_entry() {
        assert_eq!(
            requested("cargo", "serde", Some("1.0.228")).context_entry(),
            context_entry(Some("1.0.228"), None)
        );
        assert_eq!(
            requested("cargo", "serde", None).context_entry(),
            context_entry(None, Some(REQUIREMENT_ANY))
        );
        assert_eq!(REQUIREMENT_ANY, ">=0");
    }

    #[test]
    fn test_a_requested_package_refuses_an_unknown_field() {
        let parsed = serde_json::from_value::<RequestedPackage>(
            json!({ "manager": "cargo", "name": "serde", "requirement": "^1" }),
        );
        assert!(parsed.is_err(), "a read names an exact version alone");
    }

    /// The lengths the schema advertises are the constants the classifier enforces.
    #[test]
    fn test_requested_package_schema_lengths_equal_the_enforced_constants() {
        let schema = serde_json::to_value(schemars::schema_for!(RequestedPackage)).expect("schema");
        let properties = &schema["properties"];
        for (field, chars_max) in [
            ("manager", PACKAGE_MANAGER_CHARS_MAX),
            ("name", PACKAGE_NAME_CHARS_MAX),
            ("version", PACKAGE_VERSION_CHARS_MAX),
        ] {
            assert_eq!(properties[field]["minLength"], json!(1), "{field}");
            assert_eq!(properties[field]["maxLength"], json!(chars_max), "{field}");
        }
        assert_eq!(schema["required"], json!(["manager", "name"]));
    }

    /// The enum label agrees with the spelling serde emits.
    #[test]
    fn test_requested_package_violation_labels_match_serde() {
        for (violation, spelling) in [
            (RequestedPackageViolation::ManagerLength, "manager_length"),
            (RequestedPackageViolation::NameLength, "name_length"),
            (RequestedPackageViolation::VersionLength, "version_length"),
        ] {
            assert_eq!(violation.as_ref(), spelling);
            assert_eq!(
                serde_json::to_value(violation).expect("serializes"),
                json!(spelling)
            );
        }
    }
}
