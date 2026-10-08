//! Package publication bounds, separate from input and parser bounds.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ArchiveConfiguration, ByteSize, ConfigurationViolation, first_out_of_range};
use crate::index::{
    PACKAGE_DOCUMENTS_CEILING, PACKAGE_DOCUMENTS_MAX, PACKAGE_IDENTIFIER_TERMS_CEILING,
    PACKAGE_IDENTIFIER_TERMS_MAX, PACKAGE_SOURCE_BYTES_CEILING, PACKAGE_SOURCE_BYTES_MAX,
    PACKAGE_SYMBOLS_CEILING, PACKAGE_SYMBOLS_MAX, PACKAGE_UNITS_CEILING, PACKAGE_UNITS_MAX,
    PACKAGE_WARNINGS_CEILING, PACKAGE_WARNINGS_MAX,
};

/// The `[package]` table: publication capacity for exact-package collection.
/// Input files, bytes and declarations use `[source]`; parsing uses `[providers.syntax]`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_package_ranges)]
pub struct PackageConfiguration {
    /// Most source units published, 1 to 5000000.
    #[schemars(range(min = 1, max = 5_000_000))]
    pub units: u32,
    /// Most declaration records published, 1 to 50000000.
    #[schemars(range(min = 1, max = 50_000_000))]
    pub symbols: u32,
    /// Most search documents published, 1 to 55000000.
    #[schemars(range(min = 1, max = 55_000_000))]
    pub documents: u32,
    /// Most publication warnings retained, 1 to 65536.
    #[schemars(range(min = 1, max = 65_536))]
    pub warnings: u32,
    /// Most identifier terms retained per search document, 1 to 65536.
    #[schemars(range(min = 1, max = 65_536))]
    pub identifier_terms: u32,
    /// Most source bytes retained per record, 1b to 64mb; larger source is cut.
    pub retained_source: ByteSize,
    /// Most retained source bytes across units, symbols and documents, including copies.
    /// Absent preserves the existing aggregate behavior; a selected total must cover one
    /// complete record bound, and collection refuses before allocation passes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_total: Option<ByteSize>,
    /// Compressed archive admission before source selection and package analysis.
    pub archive: ArchiveConfiguration,
}

impl Default for PackageConfiguration {
    fn default() -> Self {
        Self {
            units: PACKAGE_UNITS_MAX,
            symbols: PACKAGE_SYMBOLS_MAX,
            documents: PACKAGE_DOCUMENTS_MAX,
            warnings: PACKAGE_WARNINGS_MAX,
            identifier_terms: PACKAGE_IDENTIFIER_TERMS_MAX,
            retained_source: ByteSize::from_bytes(u64::from(PACKAGE_SOURCE_BYTES_MAX)),
            retained_total: None,
            archive: ArchiveConfiguration::default(),
        }
    }
}

impl PackageConfiguration {
    /// Checks publication capacity and archive admission bounds.
    ///
    /// # Errors
    /// Returns the first bound outside its supported range.
    pub fn validate(&self) -> Result<(), ConfigurationViolation> {
        self.violation().map_or(Ok(()), Err)
    }

    pub(crate) fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "package.units",
                u64::from(self.units),
                1,
                u64::from(PACKAGE_UNITS_CEILING),
            ),
            (
                "package.symbols",
                u64::from(self.symbols),
                1,
                u64::from(PACKAGE_SYMBOLS_CEILING),
            ),
            (
                "package.documents",
                u64::from(self.documents),
                1,
                u64::from(PACKAGE_DOCUMENTS_CEILING),
            ),
            (
                "package.warnings",
                u64::from(self.warnings),
                1,
                u64::from(PACKAGE_WARNINGS_CEILING),
            ),
            (
                "package.identifier_terms",
                u64::from(self.identifier_terms),
                1,
                u64::from(PACKAGE_IDENTIFIER_TERMS_CEILING),
            ),
            (
                "package.retained_source",
                self.retained_source.bytes(),
                1,
                u64::from(PACKAGE_SOURCE_BYTES_CEILING),
            ),
        ])
        .or_else(|| {
            self.retained_total.and_then(|total| {
                first_out_of_range([(
                    "package.retained_total",
                    total.bytes(),
                    self.retained_source.bytes(),
                    u64::MAX,
                )])
            })
        })
        .or_else(|| self.archive.violation())
    }
}
