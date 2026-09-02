//! The read-only dependency index: the public declarations of cataloged packages.
//!
//! [`package_files`] is the one I/O adapter here: it walks a cataloged package's
//! source root and selects the files that spell its API. [`PackageIndex`] parses
//! those files with the shipped syntax providers and serves the package's public
//! declarations through the ranking and assembly the project index uses.
//! [`DependencyIndex`] holds every indexed package keyed by identity, with the
//! bookkeeping a background pass needs: which packages are pending, which were
//! refused, and which the catalog holds no source for.

mod failure;
#[cfg(test)]
mod fixture;
mod package;
mod walk;

pub use failure::{PackageIndexError, PackageIndexFault, PackageIndexViolation};
pub use package::PackageIndex;
pub use walk::{PackageFiles, package_files};

use std::collections::{BTreeMap, BTreeSet};

use rift_dependency::{CatalogEntry, DependencyCatalog, PackageLocation};
use rift_protocol::read::PackageIdentity;

use crate::workspace::SymbolMatch;

/// Default bound on one package's selected source bytes: 4 MiB.
pub const PACKAGE_BYTES_MAX_DEFAULT: u64 = 4 * 1024 * 1024;
/// Default bound on the bytes every indexed package holds together: 256 MiB.
pub const TOTAL_BYTES_MAX_DEFAULT: u64 = 256 * 1024 * 1024;
/// Default bound on one package's selected file count.
pub const PACKAGE_FILES_MAX_DEFAULT: usize = 2_000;
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

impl Default for DependencyIndexLimits {
    fn default() -> Self {
        Self {
            package_bytes_max: PACKAGE_BYTES_MAX_DEFAULT,
            total_bytes_max: TOTAL_BYTES_MAX_DEFAULT,
            package_files_max: PACKAGE_FILES_MAX_DEFAULT,
            directory_depth_max: DIRECTORY_DEPTH_MAX_DEFAULT,
            walk_entries_max: WALK_ENTRIES_MAX_DEFAULT,
        }
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

/// Where a package sits in the indexing pass.
/// Direct dependencies build first, then standard libraries, then everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PassOrder {
    Direct,
    Stdlib,
    Transitive,
}

impl PassOrder {
    fn of(entry: &CatalogEntry) -> Self {
        if entry.is_direct() {
            Self::Direct
        } else if entry.location() == PackageLocation::Stdlib {
            Self::Stdlib
        } else {
            Self::Transitive
        }
    }
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

/// Every indexed package keyed by identity, with the pass bookkeeping around them.
///
/// `pending` holds cataloged packages with a source root not yet indexed, in
/// pass order; `skipped` holds the ones a build refused, with the reason; and
/// `unrooted` holds the ones the catalog found no source for.
#[derive(Debug)]
pub struct DependencyIndex {
    limits: DependencyIndexLimits,
    packages: BTreeMap<IdentityKey, PackageIndex>,
    pending: Vec<PackageIdentity>,
    skipped: Vec<SkippedPackage>,
    unrooted: Vec<PackageIdentity>,
    total_bytes: u64,
}

impl DependencyIndex {
    /// An empty index with every cataloged package pending or unrooted.
    #[must_use]
    pub fn planned(catalog: &DependencyCatalog, limits: DependencyIndexLimits) -> Self {
        let mut index = Self {
            limits,
            packages: BTreeMap::new(),
            pending: Vec::new(),
            skipped: Vec::new(),
            unrooted: Vec::new(),
            total_bytes: 0,
        };
        index.queue_catalog(catalog);
        index
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
        let identity = package.identity().clone();
        self.pending.retain(|pending| pending != &identity);
        self.unrooted.retain(|unrooted| unrooted != &identity);
        self.packages.insert(key, package);
        Ok(())
    }

    /// Records that `identity` was refused for `reason`, dropping it from pending.
    pub fn skip(&mut self, identity: PackageIdentity, reason: String) {
        self.pending.retain(|pending| pending != &identity);
        self.skipped.push(SkippedPackage { identity, reason });
    }

    /// The next package the pass should build.
    #[must_use]
    pub fn next_pending(&self) -> Option<&PackageIdentity> {
        self.pending.first()
    }

    /// Follows a new catalog: drops packages it no longer lists and queues the arrivals.
    ///
    /// A package still cataloged keeps its build and its refusal; a package the
    /// catalog dropped leaves every list.
    pub fn retain_catalog(&mut self, catalog: &DependencyCatalog) {
        let cataloged: BTreeSet<IdentityKey> = catalog
            .entries()
            .iter()
            .map(|entry| identity_key(entry.identity()))
            .collect();
        let departed: Vec<IdentityKey> = self
            .packages
            .keys()
            .filter(|key| !cataloged.contains(*key))
            .cloned()
            .collect();
        for key in departed {
            if let Some(package) = self.packages.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(package.byte_count());
            }
        }
        self.skipped
            .retain(|skipped| cataloged.contains(&identity_key(&skipped.identity)));
        self.queue_catalog(catalog);
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

    /// How many cataloged packages with a source root await a build.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Every package a build refused, in refusal order.
    #[must_use]
    pub fn skipped(&self) -> &[SkippedPackage] {
        &self.skipped
    }

    /// Every cataloged package the catalog found no source root for, in pass order.
    #[must_use]
    pub fn unrooted(&self) -> &[PackageIdentity] {
        &self.unrooted
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

    /// Queues every cataloged package neither held nor refused: rooted ones pending
    /// in pass order, the rest unrooted.
    fn queue_catalog(&mut self, catalog: &DependencyCatalog) {
        let mut queued: Vec<&CatalogEntry> = catalog
            .entries()
            .iter()
            .filter(|entry| {
                !self.is_indexed(entry.identity()) && !self.is_skipped(entry.identity())
            })
            .collect();
        queued.sort_by_key(|entry| PassOrder::of(entry));
        self.pending = queued
            .iter()
            .filter(|entry| entry.source_root().is_some())
            .map(|entry| entry.identity().clone())
            .collect();
        self.unrooted = queued
            .iter()
            .filter(|entry| entry.source_root().is_none())
            .map(|entry| entry.identity().clone())
            .collect();
    }

    fn is_skipped(&self, identity: &PackageIdentity) -> bool {
        self.skipped
            .iter()
            .any(|skipped| &skipped.identity == identity)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rift_core::{ErrorCode, ErrorName};
    use rift_dependency::{CatalogEntry, DependencyCatalog, PackageLocation};
    use rift_syntax::ShippedLanguage;

    use super::fixture::{catalog, identity, language, names, rust_package, violation_of};
    use super::{
        DIRECTORY_DEPTH_MAX_DEFAULT, DependencyIndex, DependencyIndexLimits,
        PACKAGE_BYTES_MAX_DEFAULT, PACKAGE_FILES_MAX_DEFAULT, PackageIndexViolation,
        TOTAL_BYTES_MAX_DEFAULT, WALK_ENTRIES_MAX_DEFAULT,
    };
    use crate::workspace::SymbolMatchRank;

    #[test]
    fn test_limits_defaults_are_the_named_constants() {
        let limits = DependencyIndexLimits::default();
        assert_eq!(limits.package_bytes_max, PACKAGE_BYTES_MAX_DEFAULT);
        assert_eq!(limits.total_bytes_max, TOTAL_BYTES_MAX_DEFAULT);
        assert_eq!(limits.package_files_max, PACKAGE_FILES_MAX_DEFAULT);
        assert_eq!(limits.directory_depth_max, DIRECTORY_DEPTH_MAX_DEFAULT);
        assert_eq!(limits.walk_entries_max, WALK_ENTRIES_MAX_DEFAULT);
    }

    #[test]
    fn test_dependency_index_planned_orders_direct_then_stdlib_then_transitive() {
        let root = PathBuf::from("/cache");
        let catalog = catalog(vec![
            CatalogEntry::dependency(
                identity("cargo", "zeta", "1.0.0"),
                language(ShippedLanguage::Rust),
                Some(root.clone()),
                false,
            ),
            CatalogEntry::dependency(
                identity("cargo", "alpha", "1.0.0"),
                language(ShippedLanguage::Rust),
                Some(root.clone()),
                false,
            ),
            CatalogEntry::dependency(
                identity("cargo", "tokio", "1.53.1"),
                language(ShippedLanguage::Rust),
                Some(root.clone()),
                true,
            ),
            CatalogEntry::new(
                identity("stdlib", "rust", "1.90.0"),
                PackageLocation::Stdlib,
                language(ShippedLanguage::Rust),
            )
            .with_source_root(root),
            CatalogEntry::new(
                identity("cargo", "ghost", "0.1.0"),
                PackageLocation::Dependency,
                language(ShippedLanguage::Rust),
            ),
        ]);

        let index = DependencyIndex::planned(&catalog, DependencyIndexLimits::default());

        assert_eq!(index.pending_count(), 4);
        assert_eq!(
            index.next_pending().map(|identity| identity.name.as_str()),
            Some("tokio")
        );
        assert_eq!(index.unrooted(), [identity("cargo", "ghost", "0.1.0")]);
        assert_eq!(index.indexed_count(), 0);
        assert_eq!(index.total_bytes(), 0);
        assert!(index.skipped().is_empty());
        let mut order = Vec::new();
        let mut index = index;
        while let Some(next) = index.next_pending().cloned() {
            order.push(format!("{}/{}", next.manager, next.name));
            index.skip(next, "not built here".to_owned());
        }
        assert_eq!(
            order,
            ["cargo/tokio", "stdlib/rust", "cargo/alpha", "cargo/zeta"]
        );
        assert_eq!(index.skipped().len(), 4);
        assert_eq!(index.skipped()[0].reason, "not built here");
    }

    #[test]
    fn test_dependency_index_retain_catalog_drops_departed_and_queues_arrived() {
        let alpha = rust_package("alpha", "pub fn alpha() {}\n");
        let beta = rust_package("beta", "pub fn beta() {}\n");
        let held = |name: &str| {
            CatalogEntry::dependency(
                identity("cargo", name, "1.0.0"),
                language(ShippedLanguage::Rust),
                Some(PathBuf::from("/cache")),
                false,
            )
        };
        let mut index = DependencyIndex::planned(
            &catalog(vec![held("alpha"), held("beta"), held("gamma")]),
            DependencyIndexLimits::default(),
        );
        let alpha_bytes = alpha.byte_count();
        index.insert(alpha).expect("alpha fits");
        index.insert(beta).expect("beta fits");
        index.skip(identity("cargo", "gamma", "1.0.0"), "refused".to_owned());
        assert_eq!(index.pending_count(), 0);

        index.retain_catalog(&catalog(vec![held("alpha"), held("gamma"), held("delta")]));

        assert!(index.is_indexed(&identity("cargo", "alpha", "1.0.0")));
        assert!(!index.is_indexed(&identity("cargo", "beta", "1.0.0")));
        assert_eq!(index.indexed_count(), 1);
        assert_eq!(index.total_bytes(), alpha_bytes);
        assert_eq!(
            index.next_pending(),
            Some(&identity("cargo", "delta", "1.0.0"))
        );
        assert_eq!(index.pending_count(), 1);
        assert_eq!(
            index
                .skipped()
                .iter()
                .map(|skipped| skipped.identity.name.as_str())
                .collect::<Vec<_>>(),
            ["gamma"],
            "a still-cataloged refusal stays refused"
        );
        assert!(
            index
                .package(&identity("cargo", "alpha", "1.0.0"))
                .is_some()
        );
    }

    #[test]
    fn test_dependency_index_symbols_merge_across_packages_by_rank_then_identity() {
        let mut index = DependencyIndex::planned(
            &DependencyCatalog::default(),
            DependencyIndexLimits::default(),
        );
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
        let mut index = DependencyIndex::planned(&DependencyCatalog::default(), limits);
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
        let mut index = DependencyIndex::planned(
            &DependencyCatalog::default(),
            DependencyIndexLimits::default(),
        );
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
