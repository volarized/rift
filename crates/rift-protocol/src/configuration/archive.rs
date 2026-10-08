//! Archive acquisition bounds for exact-package collection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ByteSize, ConfigurationViolation, first_out_of_range};

/// Compressed archive bytes accepted by default.
pub const ARCHIVE_COMPRESSED_BYTES_DEFAULT: u64 = 64 << 20;
/// Expanded archive bytes accepted by default.
pub const ARCHIVE_EXPANDED_BYTES_DEFAULT: u64 = 512 << 20;
/// Bytes accepted in one archive member by default.
pub const ARCHIVE_MEMBER_BYTES_DEFAULT: u64 = 64 << 20;
/// Bytes in one GNU or PAX extended header when the key is absent.
pub const ARCHIVE_EXTENSION_BYTES_DEFAULT: u64 = 16 << 10;
/// Bytes in one GNU or PAX extended header, at most.
pub const ARCHIVE_EXTENSION_BYTES_MAX: u64 = 64 << 20;
/// Archive members accepted by default, including directories and skipped links.
pub const ARCHIVE_MEMBERS_DEFAULT: u32 = 100_000;
/// Expanded bytes per compressed byte accepted by default.
pub const ARCHIVE_EXPANSION_RATIO_DEFAULT: u32 = 200;
/// Largest archive byte budget, matching the supported aggregate source size.
pub const ARCHIVE_BYTES_MAX: u64 = crate::source::SOURCE_WORKSPACE_BYTES_MAX;
/// Largest archive member count, matching the supported source count.
pub const ARCHIVE_MEMBERS_MAX: u32 = 5_000_000;
/// Largest expansion ratio; the expanded byte budget still bounds allocation.
pub const ARCHIVE_EXPANSION_RATIO_MAX: u32 = u32::MAX;

/// The `[package.archive]` table bounds archive acquisition before package analysis.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_archive_ranges)]
pub struct ArchiveConfiguration {
    /// Most compressed archive bytes accepted, 1b to 64gb.
    pub compressed_size: ByteSize,
    /// Most expanded archive bytes accepted, 1b to 64gb.
    pub expanded_size: ByteSize,
    /// Most bytes accepted in one member, 1b to `expanded_size`, at most 64gb.
    pub member_size: ByteSize,
    /// Most bytes accepted in one GNU or PAX extended header, 1b to 64mb, checked before
    /// the archive reader allocates its contents. An extended header must also fit `member_size`.
    pub extension_size: ByteSize,
    /// Most archive members accepted, including directories, links, and extended headers.
    #[schemars(range(min = 1, max = 5_000_000))]
    pub members: u32,
    /// Most expanded bytes per compressed byte, before `expanded_size` bounds the result.
    #[schemars(range(min = 1, max = 4_294_967_295_u32))]
    pub expansion_ratio: u32,
}

impl Default for ArchiveConfiguration {
    fn default() -> Self {
        Self {
            compressed_size: ByteSize::from_bytes(ARCHIVE_COMPRESSED_BYTES_DEFAULT),
            expanded_size: ByteSize::from_bytes(ARCHIVE_EXPANDED_BYTES_DEFAULT),
            member_size: ByteSize::from_bytes(ARCHIVE_MEMBER_BYTES_DEFAULT),
            extension_size: ByteSize::from_bytes(ARCHIVE_EXTENSION_BYTES_DEFAULT),
            members: ARCHIVE_MEMBERS_DEFAULT,
            expansion_ratio: ARCHIVE_EXPANSION_RATIO_DEFAULT,
        }
    }
}

impl ArchiveConfiguration {
    /// Checks archive byte budgets, member count, and expansion ratio.
    ///
    /// # Errors
    /// Returns the first bound violation, including a member budget above the expanded budget.
    pub fn validate(&self) -> Result<(), ConfigurationViolation> {
        match self.violation() {
            Some(violation) => Err(violation),
            None => Ok(()),
        }
    }

    /// The first archive bound outside its supported range, in key order.
    #[must_use]
    pub(crate) fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "package.archive.compressed_size",
                self.compressed_size.bytes(),
                1,
                ARCHIVE_BYTES_MAX,
            ),
            (
                "package.archive.expanded_size",
                self.expanded_size.bytes(),
                1,
                ARCHIVE_BYTES_MAX,
            ),
            (
                "package.archive.member_size",
                self.member_size.bytes(),
                1,
                self.expanded_size.bytes().min(ARCHIVE_BYTES_MAX),
            ),
            (
                "package.archive.extension_size",
                self.extension_size.bytes(),
                1,
                ARCHIVE_EXTENSION_BYTES_MAX,
            ),
            (
                "package.archive.members",
                u64::from(self.members),
                1,
                u64::from(ARCHIVE_MEMBERS_MAX),
            ),
            (
                "package.archive.expansion_ratio",
                u64::from(self.expansion_ratio),
                1,
                u64::from(ARCHIVE_EXPANSION_RATIO_MAX),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ARCHIVE_BYTES_MAX, ARCHIVE_COMPRESSED_BYTES_DEFAULT, ARCHIVE_EXPANDED_BYTES_DEFAULT,
        ARCHIVE_EXPANSION_RATIO_DEFAULT, ARCHIVE_EXPANSION_RATIO_MAX, ARCHIVE_MEMBER_BYTES_DEFAULT,
        ARCHIVE_MEMBERS_DEFAULT, ARCHIVE_MEMBERS_MAX, ArchiveConfiguration,
    };
    use crate::configuration::{ByteSize, ConfigurationViolation};

    #[test]
    fn archive_defaults_and_supported_edges_are_accepted() {
        let default = ArchiveConfiguration::default();
        assert_eq!(
            default.compressed_size.bytes(),
            ARCHIVE_COMPRESSED_BYTES_DEFAULT
        );
        assert_eq!(
            default.expanded_size.bytes(),
            ARCHIVE_EXPANDED_BYTES_DEFAULT
        );
        assert_eq!(default.member_size.bytes(), ARCHIVE_MEMBER_BYTES_DEFAULT);
        assert_eq!(
            default.extension_size.bytes(),
            super::ARCHIVE_EXTENSION_BYTES_DEFAULT
        );
        assert_eq!(default.members, ARCHIVE_MEMBERS_DEFAULT);
        assert_eq!(default.expansion_ratio, ARCHIVE_EXPANSION_RATIO_DEFAULT);
        assert_eq!(default.validate(), Ok(()));
        for bytes in [1, ARCHIVE_BYTES_MAX] {
            let accepted = ArchiveConfiguration {
                compressed_size: ByteSize::from_bytes(bytes),
                expanded_size: ByteSize::from_bytes(bytes),
                member_size: ByteSize::from_bytes(bytes),
                extension_size: ByteSize::from_bytes(super::ARCHIVE_EXTENSION_BYTES_DEFAULT),
                members: ARCHIVE_MEMBERS_MAX,
                expansion_ratio: ARCHIVE_EXPANSION_RATIO_MAX,
            };
            assert_eq!(accepted.validate(), Ok(()));
        }
    }

    #[test]
    fn archive_byte_budgets_and_counts_name_invalid_keys() {
        type Setter = fn(&mut ArchiveConfiguration, u64);
        let cases: [(&str, Setter, u64); 6] = [
            (
                "compressed_size",
                |table, value| table.compressed_size = ByteSize::from_bytes(value),
                ARCHIVE_BYTES_MAX + 1,
            ),
            (
                "expanded_size",
                |table, value| table.expanded_size = ByteSize::from_bytes(value),
                ARCHIVE_BYTES_MAX + 1,
            ),
            (
                "member_size",
                |table, value| table.member_size = ByteSize::from_bytes(value),
                ARCHIVE_MEMBER_BYTES_DEFAULT + ARCHIVE_EXPANDED_BYTES_DEFAULT,
            ),
            (
                "extension_size",
                |table, value| table.extension_size = ByteSize::from_bytes(value),
                super::ARCHIVE_EXTENSION_BYTES_MAX + 1,
            ),
            (
                "members",
                |table, value| table.members = u32::try_from(value).expect("bounded fixture"),
                u64::from(ARCHIVE_MEMBERS_MAX) + 1,
            ),
            (
                "expansion_ratio",
                |table, value| {
                    table.expansion_ratio = u32::try_from(value).expect("bounded fixture");
                },
                0,
            ),
        ];
        for (key, set, excessive) in cases {
            for value in [0, excessive] {
                let mut configuration = ArchiveConfiguration::default();
                set(&mut configuration, value);
                let violation = configuration
                    .validate()
                    .expect_err("invalid budget must refuse");
                assert!(
                    matches!(violation, ConfigurationViolation::LimitOutOfRange { field, .. } if field.ends_with(key)),
                    "{key}: {violation:?}"
                );
            }
        }
    }

    #[test]
    fn archive_schema_ranges_match_runtime_limits() {
        let schema = schemars::schema_for!(ArchiveConfiguration).to_value();
        let properties = &schema["properties"];
        for key in ["compressed_size", "expanded_size", "member_size"] {
            assert_eq!(
                properties[key]["rift:range"],
                serde_json::json!({ "min": "1b", "max": "64gb" })
            );
        }
        for (key, maximum) in [
            ("members", ARCHIVE_MEMBERS_MAX),
            ("expansion_ratio", ARCHIVE_EXPANSION_RATIO_MAX),
        ] {
            assert_eq!(properties[key]["minimum"], serde_json::json!(1));
            assert_eq!(properties[key]["maximum"], serde_json::json!(maximum));
        }
        assert_eq!(
            properties["extension_size"]["default"],
            serde_json::json!("16kb")
        );
        assert_eq!(
            properties["extension_size"]["rift:range"],
            serde_json::json!({"min": "1b", "max": "64mb"})
        );
        for bytes in [1, super::ARCHIVE_EXTENSION_BYTES_MAX] {
            let accepted = ArchiveConfiguration {
                extension_size: ByteSize::from_bytes(bytes),
                ..ArchiveConfiguration::default()
            };
            assert_eq!(accepted.validate(), Ok(()));
        }
        assert_eq!(
            u64::from(ARCHIVE_MEMBERS_MAX),
            crate::source::SOURCE_FILES_MAX
        );
    }
}
