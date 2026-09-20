//! The read-only dependency index: the public declarations of cataloged packages.
//!
//! [`package_files`] is the one I/O adapter here: it walks a cataloged package's
//! source root and selects the files that spell its API. [`PackageIndex`] parses
//! those files with the shipped syntax providers and serves the package's public
//! declarations through the ranking and assembly the project index uses.
//! [`PackageAnalyzer`] turns those files into one canonical
//! [`PackagePublication`](rift_protocol::index::PackagePublication), and
//! [`DependencyIndex`] holds every analyzed package keyed by identity, beside the ones
//! a fill refused and the reason each was refused.

mod analyzer;
mod failure;
#[cfg(test)]
mod fixture;
mod manifest;
mod package;
mod walk;

pub use analyzer::{AnalyzedFile, PackageAnalysis, PackageAnalyzer};
pub use failure::{PackageIndexError, PackageIndexFault, PackageIndexViolation};
pub use manifest::{
    ManifestError, analyzer_manifest_path, analyzer_revision, render_analyzer_manifest,
};
pub use package::PackageIndex;
pub use walk::{PackageFiles, package_files};

use std::collections::BTreeMap;

use rift_protocol::dependencies::DependenciesConfiguration;
use rift_protocol::read::PackageIdentity;

use crate::workspace::SymbolMatch;

/// Default bound on directory depth below one package's source root.
pub const DIRECTORY_DEPTH_MAX_DEFAULT: usize = 16;
/// Default bound on the directory entries one package walk examines.
pub const WALK_ENTRIES_MAX_DEFAULT: usize = 50_000;

/// The `limit` field a package byte refusal names.
const PACKAGE_BYTES_MAX_FIELD: &str = "package_bytes_max";
/// The `limit` field a total byte refusal names.
const TOTAL_BYTES_MAX_FIELD: &str = "total_bytes_max";
/// The `limit` field a package file-count refusal names.
const PACKAGE_FILES_MAX_FIELD: &str = "package_files_max";
/// The `limit` field a directory depth refusal names.
const DIRECTORY_DEPTH_MAX_FIELD: &str = "directory_depth_max";
/// The `limit` field a walk entry refusal names.
const WALK_ENTRIES_MAX_FIELD: &str = "walk_entries_max";

/// Bounds on what one dependency index reads and holds.
///
/// `package_bytes_max` and `package_files_max` bound one package's selected
/// files; `directory_depth_max` and `walk_entries_max` bound the walk that
/// selects them; `total_bytes_max` bounds every indexed package together.
/// The `[dependencies]` table sets the first three; the walk bounds are this
/// crate's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DependencyIndexLimits {
    /// Most selected source bytes one package may hold.
    pub package_bytes_max: u64,
    /// Most bytes every indexed package may hold together.
    pub total_bytes_max: u64,
    /// Most selected files one package may hold.
    pub package_files_max: usize,
    /// Deepest directory below a source root the walk descends into.
    pub directory_depth_max: usize,
    /// Most directory entries one package walk examines, selected or not.
    pub walk_entries_max: usize,
}

impl From<&DependenciesConfiguration> for DependencyIndexLimits {
    /// The table's `package_size`, `index_size`, and `package_files` under this
    /// crate's walk bounds. A file count past `usize` saturates, which the table's
    /// own ceiling keeps unreachable.
    fn from(configuration: &DependenciesConfiguration) -> Self {
        Self {
            package_bytes_max: configuration.package_size.bytes(),
            total_bytes_max: configuration.index_size.bytes(),
            package_files_max: usize::try_from(configuration.package_files).unwrap_or(usize::MAX),
            directory_depth_max: DIRECTORY_DEPTH_MAX_DEFAULT,
            walk_entries_max: WALK_ENTRIES_MAX_DEFAULT,
        }
    }
}

impl Default for DependencyIndexLimits {
    /// The default `[dependencies]` table's bounds.
    fn default() -> Self {
        Self::from(&DependenciesConfiguration::default())
    }
}

/// One package the index refused, with the refusal's text.
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedPackage {
    /// The refused package.
    pub identity: PackageIdentity,
    /// Why it was refused, as the error rendered it.
    pub reason: String,
}

/// One declaration match from one indexed package.
#[derive(Debug, Clone, Copy)]
pub struct DependencySymbolMatch<'a> {
    /// The package the declaration belongs to.
    pub package: &'a PackageIndex,
    /// The matched declaration and its file.
    pub matched: SymbolMatch<'a>,
}

/// The catalog's identity order: manager, then name, then version.
type IdentityKey = (String, String, String);

fn identity_key(identity: &PackageIdentity) -> IdentityKey {
    (
        identity.manager.clone(),
        identity.name.clone(),
        identity.version.clone(),
    )
}

/// Every analyzed package keyed by identity, and the ones a fill refused.
///
/// One fill analyzes the packages the dependency context names and inserts them here;
/// `skipped` holds the ones the walk or the analysis refused, with the reason a read
/// reports.
#[derive(Debug)]
pub struct DependencyIndex {
    limits: DependencyIndexLimits,
    packages: BTreeMap<IdentityKey, PackageIndex>,
    skipped: Vec<SkippedPackage>,
    total_bytes: u64,
}

impl DependencyIndex {
    /// An index holding no package, under `limits`.
    #[must_use]
    pub fn empty(limits: DependencyIndexLimits) -> Self {
        Self {
            limits,
            packages: BTreeMap::new(),
            skipped: Vec::new(),
            total_bytes: 0,
        }
    }

    /// Holds one built package, replacing an earlier build of the same identity.
    ///
    /// # Errors
    ///
    /// Returns [`PackageIndexError`] naming the package when holding it would
    /// exceed `total_bytes_max`; the index is unchanged.
    pub fn insert(&mut self, package: PackageIndex) -> Result<(), PackageIndexError> {
        let key = identity_key(package.identity());
        let standing = self.packages.get(&key).map_or(0, PackageIndex::byte_count);
        let total = self
            .total_bytes
            .saturating_sub(standing)
            .saturating_add(package.byte_count());
        if total > self.limits.total_bytes_max {
            return Err(PackageIndexFault::new(
                PackageIndexViolation::TotalBytesExceeded,
                package.identity(),
            )
            .breached(TOTAL_BYTES_MAX_FIELD, self.limits.total_bytes_max, total)
            .into());
        }
        self.total_bytes = total;
        self.packages.insert(key, package);
        Ok(())
    }

    /// Records that `identity` was refused for `reason`.
    pub fn skip(&mut self, identity: PackageIdentity, reason: String) {
        self.skipped.push(SkippedPackage { identity, reason });
    }

    /// Public declarations matching `query` across every indexed package.
    ///
    /// Merged by rank, then package identity, then qualified name, and cut to
    /// `limit`; each package contributes at most `limit` of its own.
    #[must_use]
    pub fn symbols(&self, query: &str, limit: usize) -> Vec<DependencySymbolMatch<'_>> {
        let mut matches: Vec<DependencySymbolMatch<'_>> = self
            .packages
            .values()
            .flat_map(|package| {
                package
                    .symbols(query, limit)
                    .into_iter()
                    .map(move |matched| DependencySymbolMatch { package, matched })
            })
            .collect();
        matches.sort_by_cached_key(|found| {
            (
                found.matched.rank,
                identity_key(found.package.identity()),
                found.matched.symbol.qualified_name.clone(),
            )
        });
        matches.truncate(limit);
        matches
    }

    /// Every indexed package, in identity order.
    #[must_use]
    pub fn packages(&self) -> impl ExactSizeIterator<Item = &PackageIndex> {
        self.packages.values()
    }

    /// The indexed package with `identity`, when one is held.
    #[must_use]
    pub fn package(&self, identity: &PackageIdentity) -> Option<&PackageIndex> {
        self.packages.get(&identity_key(identity))
    }

    /// Whether a build of `identity` is held.
    #[must_use]
    pub fn is_indexed(&self, identity: &PackageIdentity) -> bool {
        self.packages.contains_key(&identity_key(identity))
    }

    /// Every package a build refused, in refusal order.
    #[must_use]
    pub fn skipped(&self) -> &[SkippedPackage] {
        &self.skipped
    }

    /// How many packages are held.
    #[must_use]
    pub fn indexed_count(&self) -> usize {
        self.packages.len()
    }

    /// The bytes every held package holds together.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// The bounds every held build was read under.
    #[must_use]
    pub const fn limits(&self) -> DependencyIndexLimits {
        self.limits
    }
}

#[cfg(test)]
mod tests {

    use rift_core::{ErrorCode, ErrorName};
    use rift_protocol::configuration::ByteSize;
    use rift_protocol::dependencies::DependenciesConfiguration;

    use super::fixture::{identity, names, rust_package, violation_of};
    use super::{
        DIRECTORY_DEPTH_MAX_DEFAULT, DependencyIndex, DependencyIndexLimits, PackageIndexViolation,
        WALK_ENTRIES_MAX_DEFAULT,
    };
    use crate::workspace::SymbolMatchRank;

    #[test]
    fn test_limits_follow_the_dependencies_table_under_the_walk_bounds() {
        let table = DependenciesConfiguration {
            package_size: ByteSize::from_bytes(1 << 20),
            index_size: ByteSize::from_bytes(8 << 20),
            package_files: 12,
            ..DependenciesConfiguration::default()
        };
        let limits = DependencyIndexLimits::from(&table);
        assert_eq!(limits.package_bytes_max, 1 << 20);
        assert_eq!(limits.total_bytes_max, 8 << 20);
        assert_eq!(limits.package_files_max, 12);
        assert_eq!(limits.directory_depth_max, DIRECTORY_DEPTH_MAX_DEFAULT);
        assert_eq!(limits.walk_entries_max, WALK_ENTRIES_MAX_DEFAULT);
        assert_eq!(
            DependencyIndexLimits::default(),
            DependencyIndexLimits::from(&DependenciesConfiguration::default())
        );
    }

    #[test]
    fn test_dependency_index_symbols_merge_across_packages_by_rank_then_identity() {
        let mut index = DependencyIndex::empty(DependencyIndexLimits::default());
        index
            .insert(rust_package("zeta", "pub fn spawn() {}\n"))
            .expect("zeta fits");
        index
            .insert(rust_package(
                "alpha",
                "pub fn spawn_blocking() {}\npub fn spawn() {}\n",
            ))
            .expect("alpha fits");

        let matches = index.symbols("spawn", 10);

        let found: Vec<(&str, &str, SymbolMatchRank)> = matches
            .iter()
            .map(|found| {
                (
                    found.package.identity().name.as_str(),
                    found.matched.symbol.qualified_name.as_str(),
                    found.matched.rank,
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                ("alpha", "spawn", SymbolMatchRank::QualifiedExact),
                ("zeta", "spawn", SymbolMatchRank::QualifiedExact),
                ("alpha", "spawn_blocking", SymbolMatchRank::NamePrefix),
            ]
        );
        assert_eq!(
            index.symbols("spawn", 2).len(),
            2,
            "the merge is cut to the limit"
        );
        assert_eq!(index.packages().count(), 2);
    }

    #[test]
    fn test_dependency_index_insert_past_total_bytes_max_refuses_naming_the_package() {
        let alpha = rust_package("alpha", "pub fn alpha() {}\n");
        let beta = rust_package("beta", "pub fn beta() {}\n");
        let limits = DependencyIndexLimits {
            total_bytes_max: alpha.byte_count(),
            ..DependencyIndexLimits::default()
        };
        let mut index = DependencyIndex::empty(limits);
        index.insert(alpha).expect("exactly the bound is accepted");

        let error = index
            .insert(beta)
            .expect_err("a second package crosses the bound");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::TotalBytesExceeded
        );
        assert_eq!(error.fault().package(), &identity("cargo", "beta", "1.0.0"));
        assert!(error.to_string().contains("cargo/beta@1.0.0"));
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::LimitExceeded));
        assert_eq!(index.indexed_count(), 1, "the refused package is not held");
        assert_eq!(index.total_bytes(), limits.total_bytes_max);
    }

    #[test]
    fn test_dependency_index_insert_replaces_an_earlier_build_of_one_identity() {
        let mut index = DependencyIndex::empty(DependencyIndexLimits::default());
        index
            .insert(rust_package("alpha", "pub fn first() {}\n"))
            .expect("first build");
        let second = rust_package("alpha", "pub fn second_longer_name() {}\n");
        let second_bytes = second.byte_count();

        index.insert(second).expect("second build");

        assert_eq!(index.indexed_count(), 1);
        assert_eq!(index.total_bytes(), second_bytes);
        assert_eq!(
            names(
                &index
                    .package(&identity("cargo", "alpha", "1.0.0"))
                    .expect("held")
                    .symbols("", 5)
            ),
            ["second_longer_name"]
        );
    }
}
