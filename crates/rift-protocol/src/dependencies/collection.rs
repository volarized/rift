//! Bounds for dependency manifests, lockfiles, installed files, and version probes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::configuration::{ByteSize, ConfigurationViolation, first_out_of_range};

/// Largest file read by dependency collection, matching the supported syntax file size.
pub const DEPENDENCIES_COLLECTION_BYTES_MAX: u64 = crate::configuration::SYNTAX_FILE_BYTES_MAX;
/// Largest retained version probe output, below the shared stream drain ceiling.
pub const DEPENDENCIES_COLLECTION_OUTPUT_MAX: u64 = 1 << 20;
/// Largest number of manifests or entries one dependency directory listing carries.
pub const DEPENDENCIES_COLLECTION_COUNT_MAX: u32 = 5_000_000;
/// Largest manifest list, bounding the existing quadratic ancestor comparisons.
pub const DEPENDENCIES_COLLECTION_MANIFESTS_MAX: u32 = 4096;
/// Largest number of names one Bun package key nests.
pub const DEPENDENCIES_COLLECTION_NESTING_MAX: u32 = 1024;

/// The `[dependencies.collection]` table bounds dependency input reads and package collection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_dependencies_collection_ranges)]
pub struct DependenciesCollectionConfiguration {
    /// Most bytes read from one dependency manifest or lockfile, 1b to 64mb.
    pub lockfile_size: ByteSize,
    /// Most bytes kept from one version probe stream, 1b to 1mb.
    pub toolchain_output: ByteSize,
    /// Most manifests read by one resolver; ancestor comparisons use at most `manifests` squared work.
    #[schemars(range(min = 1, max = 4096))]
    pub manifests: u32,
    /// Most packages held in one dependency context, within the global API's 20,000 package ceiling.
    #[schemars(range(min = 1, max = 20_000))]
    pub packages: u32,
    /// Most entries returned by one installed environment directory listing.
    #[schemars(range(min = 1, max = 5_000_000))]
    pub directory_entries: u32,
    /// Most bytes read from one toolchain or version pin file, 1b to 64mb.
    pub pin_size: ByteSize,
    /// Most bytes read from one standard library project file, 1b to 64mb.
    pub project_size: ByteSize,
    /// Most bytes read from one installed distribution's `RECORD`, 1b to 64mb.
    pub record_size: ByteSize,
    /// Most package names one Bun package key nests.
    #[schemars(range(min = 1, max = 1024))]
    pub nesting_depth: u32,
}

impl Default for DependenciesCollectionConfiguration {
    fn default() -> Self {
        Self {
            lockfile_size: ByteSize::from_bytes(16 << 20),
            toolchain_output: ByteSize::from_bytes(64 << 10),
            manifests: 256,
            packages: 20_000,
            directory_entries: 16_384,
            pin_size: ByteSize::from_bytes(64 << 10),
            project_size: ByteSize::from_bytes(1 << 20),
            record_size: ByteSize::from_bytes(4 << 20),
            nesting_depth: 32,
        }
    }
}

impl DependenciesCollectionConfiguration {
    /// Checks dependency collection bounds before a resolver reads any input.
    ///
    /// # Errors
    /// Returns the first key outside its advertised range.
    pub fn validate(&self) -> Result<(), ConfigurationViolation> {
        match self.violation() {
            Some(violation) => Err(violation),
            None => Ok(()),
        }
    }

    pub(crate) fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "dependencies.collection.lockfile_size",
                self.lockfile_size.bytes(),
                1,
                DEPENDENCIES_COLLECTION_BYTES_MAX,
            ),
            (
                "dependencies.collection.toolchain_output",
                self.toolchain_output.bytes(),
                1,
                DEPENDENCIES_COLLECTION_OUTPUT_MAX,
            ),
            (
                "dependencies.collection.manifests",
                u64::from(self.manifests),
                1,
                u64::from(DEPENDENCIES_COLLECTION_MANIFESTS_MAX),
            ),
            (
                "dependencies.collection.packages",
                u64::from(self.packages),
                1,
                20_000,
            ),
            (
                "dependencies.collection.directory_entries",
                u64::from(self.directory_entries),
                1,
                u64::from(DEPENDENCIES_COLLECTION_COUNT_MAX),
            ),
            (
                "dependencies.collection.pin_size",
                self.pin_size.bytes(),
                1,
                DEPENDENCIES_COLLECTION_BYTES_MAX,
            ),
            (
                "dependencies.collection.project_size",
                self.project_size.bytes(),
                1,
                DEPENDENCIES_COLLECTION_BYTES_MAX,
            ),
            (
                "dependencies.collection.record_size",
                self.record_size.bytes(),
                1,
                DEPENDENCIES_COLLECTION_BYTES_MAX,
            ),
            (
                "dependencies.collection.nesting_depth",
                u64::from(self.nesting_depth),
                1,
                u64::from(DEPENDENCIES_COLLECTION_NESTING_MAX),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_collection_schema_and_runtime_accept_the_same_edges() {
        type Setter = fn(&mut DependenciesCollectionConfiguration, u64);
        let cases: [(&str, Setter, u64); 9] = [
            (
                "lockfile_size",
                |value, number| value.lockfile_size = ByteSize::from_bytes(number),
                DEPENDENCIES_COLLECTION_BYTES_MAX,
            ),
            (
                "toolchain_output",
                |value, number| value.toolchain_output = ByteSize::from_bytes(number),
                DEPENDENCIES_COLLECTION_OUTPUT_MAX,
            ),
            (
                "manifests",
                |value, number| value.manifests = u32::try_from(number).expect("bounded fixture"),
                u64::from(DEPENDENCIES_COLLECTION_MANIFESTS_MAX),
            ),
            (
                "packages",
                |value, number| value.packages = u32::try_from(number).expect("bounded fixture"),
                20_000,
            ),
            (
                "directory_entries",
                |value, number| {
                    value.directory_entries = u32::try_from(number).expect("bounded fixture");
                },
                u64::from(DEPENDENCIES_COLLECTION_COUNT_MAX),
            ),
            (
                "pin_size",
                |value, number| value.pin_size = ByteSize::from_bytes(number),
                DEPENDENCIES_COLLECTION_BYTES_MAX,
            ),
            (
                "project_size",
                |value, number| value.project_size = ByteSize::from_bytes(number),
                DEPENDENCIES_COLLECTION_BYTES_MAX,
            ),
            (
                "record_size",
                |value, number| value.record_size = ByteSize::from_bytes(number),
                DEPENDENCIES_COLLECTION_BYTES_MAX,
            ),
            (
                "nesting_depth",
                |value, number| {
                    value.nesting_depth = u32::try_from(number).expect("bounded fixture");
                },
                u64::from(DEPENDENCIES_COLLECTION_NESTING_MAX),
            ),
        ];
        let schema = schemars::schema_for!(DependenciesCollectionConfiguration).to_value();
        assert_eq!(
            DependenciesCollectionConfiguration::default().validate(),
            Ok(())
        );
        for (key, set, maximum) in cases {
            for accepted in [1, maximum] {
                let mut table = DependenciesCollectionConfiguration::default();
                set(&mut table, accepted);
                assert_eq!(table.validate(), Ok(()), "{key}: {accepted}");
            }
            for refused in [0, maximum + 1] {
                let mut table = DependenciesCollectionConfiguration::default();
                set(&mut table, refused);
                assert!(
                    matches!(table.validate(), Err(ConfigurationViolation::LimitOutOfRange { field, value, min: 1, max }) if field.ends_with(key) && value == refused && max == maximum)
                );
            }
            let property = &schema["properties"][key];
            if key.ends_with("size") || key == "toolchain_output" {
                assert_eq!(
                    property["rift:range"],
                    serde_json::json!({"min": ByteSize::from_bytes(1), "max": ByteSize::from_bytes(maximum)})
                );
            } else {
                assert_eq!(property["minimum"], serde_json::json!(1));
                assert_eq!(property["maximum"], serde_json::json!(maximum));
            }
        }
    }
}
