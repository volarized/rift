//! The dependency catalog: the packages a workspace's toolchains resolved, keyed by identity.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rift_protocol::read::{Language, PackageIdentity, ProjectPath};

use crate::resolver::{
    DependencyResolver, Inspector, MANIFESTS_MAX, ResolutionRequest, ResolverName,
};

/// Where a cataloged package's source belongs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageLocation {
    /// One resolved dependency of a workspace manifest.
    Dependency,
    /// The standard library of a toolchain the workspace resolved against.
    Stdlib,
}

/// One package a resolver cataloged.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogEntry {
    identity: PackageIdentity,
    location: PackageLocation,
    source_root: Option<PathBuf>,
    direct: bool,
    language: Language,
}

impl CatalogEntry {
    /// One entry with no source root, not declared directly.
    #[must_use]
    pub const fn new(
        identity: PackageIdentity,
        location: PackageLocation,
        language: Language,
    ) -> Self {
        Self {
            identity,
            location,
            source_root: None,
            direct: false,
            language,
        }
    }

    /// One dependency entry with its source root and whether a manifest declares it.
    #[must_use]
    pub const fn dependency(
        identity: PackageIdentity,
        language: Language,
        source_root: Option<PathBuf>,
        declared_directly: bool,
    ) -> Self {
        Self {
            identity,
            location: PackageLocation::Dependency,
            source_root,
            direct: declared_directly,
            language,
        }
    }

    /// Records the directory holding the package's source on this machine.
    #[must_use]
    pub fn with_source_root(mut self, source_root: PathBuf) -> Self {
        self.source_root = Some(source_root);
        self
    }

    /// Marks the package as declared directly by a workspace manifest.
    #[must_use]
    pub const fn declared_directly(mut self) -> Self {
        self.direct = true;
        self
    }

    /// The package as its manager identifies it.
    #[must_use]
    pub const fn identity(&self) -> &PackageIdentity {
        &self.identity
    }

    /// Whether the package is a dependency or a standard library.
    #[must_use]
    pub const fn location(&self) -> PackageLocation {
        self.location
    }

    /// The directory holding the package's source, absent when no cache on this machine
    /// holds its bytes.
    #[must_use]
    pub fn source_root(&self) -> Option<&Path> {
        self.source_root.as_deref()
    }

    /// Whether a workspace manifest declares the package directly.
    #[must_use]
    pub const fn is_direct(&self) -> bool {
        self.direct
    }

    /// The language whose syntax provider parses the package's source.
    #[must_use]
    pub const fn language(&self) -> &Language {
        &self.language
    }
}

/// Why one resolver answered less than its toolchain would have.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Degradation {
    /// The resolver that degraded.
    pub resolver: ResolverName,
    /// What it could not do, and what it answered instead.
    pub reason: String,
}

/// One resolver's answer for one workspace.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resolution {
    /// The packages the resolver cataloged.
    pub entries: Vec<CatalogEntry>,
    /// The visible workspace paths the resolver read. A change to any of them makes the
    /// answer stale.
    pub inputs: Vec<ProjectPath>,
    /// Everything the resolver could not do, in the order it met each.
    pub degradations: Vec<String>,
}

/// The packages every resolver cataloged for one workspace, keyed by identity.
///
/// Assembly sorts entries by `(manager, name, version)` and merges two entries of one
/// identity into one: a source root survives from whichever entry carried it, and the
/// package is direct when either said so.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DependencyCatalog {
    entries: Vec<CatalogEntry>,
    inputs: BTreeSet<ProjectPath>,
    degradations: Vec<Degradation>,
}

impl DependencyCatalog {
    /// Merges every resolver's answer into one catalog.
    #[must_use]
    pub fn assemble(resolutions: Vec<(ResolverName, Resolution)>) -> Self {
        let mut merged: BTreeMap<(String, String, String), CatalogEntry> = BTreeMap::new();
        let mut inputs = BTreeSet::new();
        let mut degradations = Vec::new();
        for (resolver, resolution) in resolutions {
            for entry in resolution.entries {
                let key = identity_key(&entry.identity);
                match merged.get_mut(&key) {
                    Some(standing) => merge_into(standing, entry),
                    None => {
                        merged.insert(key, entry);
                    }
                }
            }
            inputs.extend(resolution.inputs);
            degradations.extend(
                resolution
                    .degradations
                    .into_iter()
                    .map(|reason| Degradation { resolver, reason }),
            );
        }
        Self {
            entries: merged.into_values().collect(),
            inputs,
            degradations,
        }
    }

    /// Every cataloged package, in identity order.
    #[must_use]
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// The packages workspace manifests declare directly, in identity order.
    pub fn direct_packages(&self) -> impl Iterator<Item = &PackageIdentity> {
        self.entries
            .iter()
            .filter(|entry| entry.is_direct())
            .map(CatalogEntry::identity)
    }

    /// The visible workspace paths the catalog was resolved from, in path order.
    pub fn inputs(&self) -> impl Iterator<Item = &ProjectPath> {
        self.inputs.iter()
    }

    /// Whether a change to `path` makes this catalog stale.
    #[must_use]
    pub fn depends_on(&self, path: &ProjectPath) -> bool {
        self.inputs.contains(path)
    }

    /// Everything the resolvers could not do, in resolver order.
    #[must_use]
    pub fn degradations(&self) -> &[Degradation] {
        &self.degradations
    }

    /// Whether any resolver answered less than its toolchain would have. A degraded
    /// catalog is resolved again on the next whole rebuild, since the machine may have
    /// gained the toolchain or the cache since.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        !self.degradations.is_empty()
    }
}

/// Runs every resolver over one workspace and assembles what they cataloged.
///
/// Each resolver receives the visible paths carrying its manifest file name, at most
/// [`MANIFESTS_MAX`] of them in path order; a workspace with more reports the drop as a
/// degradation. A resolver claiming no visible manifest does not run. The work is
/// proportional to the visible path count plus what each resolver reads through
/// `inspector`.
pub fn resolve_catalog(
    root: &Path,
    visible: &[ProjectPath],
    resolvers: &[&dyn DependencyResolver],
    inspector: &mut dyn Inspector,
) -> DependencyCatalog {
    let mut resolutions = Vec::with_capacity(resolvers.len());
    for resolver in resolvers {
        let claimed: Vec<ProjectPath> = visible
            .iter()
            .filter(|path| file_name(path) == resolver.manifest_file_name())
            .cloned()
            .collect();
        if claimed.is_empty() {
            continue;
        }
        let (manifests, dropped) = if claimed.len() > MANIFESTS_MAX {
            (&claimed[..MANIFESTS_MAX], claimed.len() - MANIFESTS_MAX)
        } else {
            (&claimed[..], 0)
        };
        let request = ResolutionRequest { root, manifests };
        let mut resolution = resolver.resolve(&request, inspector);
        if dropped > 0 {
            resolution.degradations.push(format!(
                "{dropped} of {} {} manifests were not read: at most {MANIFESTS_MAX} are \
                 read per workspace",
                claimed.len(),
                resolver.manifest_file_name()
            ));
        }
        resolutions.push((resolver.name(), resolution));
    }
    DependencyCatalog::assemble(resolutions)
}

/// The last segment of a project path: the file name a resolver claims a manifest by.
#[must_use]
pub(crate) fn file_name(path: &ProjectPath) -> &str {
    path.0.rsplit('/').next().unwrap_or(path.0.as_str())
}

/// One package identity from its three borrowed parts.
#[must_use]
pub(crate) fn package_identity(manager: &str, name: &str, version: &str) -> PackageIdentity {
    PackageIdentity {
        manager: manager.to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
    }
}

/// The package namespace of a toolchain's standard library, whatever the language.
pub(crate) const STDLIB_MANAGER: &str = "stdlib";

fn identity_key(identity: &PackageIdentity) -> (String, String, String) {
    (
        identity.manager.clone(),
        identity.name.clone(),
        identity.version.clone(),
    )
}

/// Folds a second entry of one identity into the standing one.
fn merge_into(standing: &mut CatalogEntry, entry: CatalogEntry) {
    if standing.source_root.is_none() {
        standing.source_root = entry.source_root;
    }
    standing.direct |= entry.direct;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, version: &str) -> PackageIdentity {
        PackageIdentity {
            manager: "cargo".to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }

    fn rust() -> Language {
        Language::from_identity_segment("rust").expect("rust is a language segment")
    }

    #[test]
    fn test_assemble_sorts_by_identity_and_merges_duplicates() {
        let first = Resolution {
            entries: vec![
                CatalogEntry::new(
                    identity("tokio", "1.53.1"),
                    PackageLocation::Dependency,
                    rust(),
                ),
                CatalogEntry::new(
                    identity("serde", "1.0.228"),
                    PackageLocation::Dependency,
                    rust(),
                )
                .declared_directly(),
            ],
            inputs: vec![ProjectPath("Cargo.toml".to_owned())],
            degradations: Vec::new(),
        };
        let second = Resolution {
            entries: vec![
                CatalogEntry::new(
                    identity("tokio", "1.53.1"),
                    PackageLocation::Dependency,
                    rust(),
                )
                .with_source_root(PathBuf::from("/cache/tokio-1.53.1"))
                .declared_directly(),
            ],
            inputs: vec![ProjectPath("Cargo.lock".to_owned())],
            degradations: vec!["static answer".to_owned()],
        };

        let catalog = DependencyCatalog::assemble(vec![
            (ResolverName::Cargo, first),
            (ResolverName::Cargo, second),
        ]);

        let names: Vec<&str> = catalog
            .entries()
            .iter()
            .map(|entry| entry.identity().name.as_str())
            .collect();
        assert_eq!(names, ["serde", "tokio"], "entries sort by identity");
        let tokio = &catalog.entries()[1];
        assert_eq!(tokio.source_root(), Some(Path::new("/cache/tokio-1.53.1")));
        assert!(
            tokio.is_direct(),
            "direct survives the merge from either side"
        );
        assert_eq!(catalog.direct_packages().count(), 2);
        assert!(catalog.depends_on(&ProjectPath("Cargo.lock".to_owned())));
        assert!(!catalog.depends_on(&ProjectPath("src/lib.rs".to_owned())));
        assert!(catalog.is_degraded());
        assert_eq!(catalog.degradations()[0].resolver, ResolverName::Cargo);
    }

    #[test]
    fn test_empty_catalog_is_not_degraded_and_lists_nothing() {
        let catalog = DependencyCatalog::default();
        assert!(catalog.entries().is_empty());
        assert!(!catalog.is_degraded());
        assert_eq!(catalog.inputs().count(), 0);
    }

    /// A resolver claiming `probe.toml` that answers one entry per manifest it was handed.
    #[derive(Debug)]
    struct ProbeResolver;

    impl DependencyResolver for ProbeResolver {
        fn name(&self) -> ResolverName {
            ResolverName::Cargo
        }

        fn manager(&self) -> &'static str {
            "probe"
        }

        fn language(&self) -> Language {
            rust()
        }

        fn manifest_file_name(&self) -> &'static str {
            "probe.toml"
        }

        fn resolve(
            &self,
            request: &ResolutionRequest<'_>,
            _inspector: &mut dyn Inspector,
        ) -> Resolution {
            let entries = request
                .manifests
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    let identity = identity(&format!("package-{index:04}"), "1.0.0");
                    CatalogEntry::new(identity, PackageLocation::Dependency, rust())
                })
                .collect();
            Resolution {
                entries,
                inputs: request.manifests.to_vec(),
                degradations: Vec::new(),
            }
        }
    }

    fn probe_manifests(count: usize) -> Vec<ProjectPath> {
        (0..count)
            .map(|index| ProjectPath(format!("packages/p{index:04}/probe.toml")))
            .collect()
    }

    #[test]
    fn test_resolve_catalog_hands_each_resolver_its_claimed_manifests() {
        let visible = vec![
            ProjectPath("probe.toml".to_owned()),
            ProjectPath("src/lib.rs".to_owned()),
            ProjectPath("tools/probe.toml".to_owned()),
        ];
        let mut inspector = crate::fixture::RecordedInspector::default();

        let catalog = resolve_catalog(
            Path::new("/workspace"),
            &visible,
            &[&ProbeResolver],
            &mut inspector,
        );

        assert_eq!(catalog.entries().len(), 2);
        let inputs: Vec<&str> = catalog.inputs().map(|path| path.0.as_str()).collect();
        assert_eq!(inputs, ["probe.toml", "tools/probe.toml"]);
        assert!(!catalog.is_degraded());
    }

    #[test]
    fn test_resolve_catalog_skips_a_resolver_with_no_claimed_manifest() {
        let visible = vec![ProjectPath("Cargo.toml".to_owned())];
        let mut inspector = crate::fixture::RecordedInspector::default();

        let catalog = resolve_catalog(
            Path::new("/workspace"),
            &visible,
            &[&ProbeResolver],
            &mut inspector,
        );

        assert!(catalog.entries().is_empty());
        assert_eq!(catalog.inputs().count(), 0);
        assert!(!catalog.is_degraded());
    }

    #[test]
    fn test_resolve_catalog_reads_exactly_manifests_max_without_a_drop() {
        let visible = probe_manifests(MANIFESTS_MAX);
        let mut inspector = crate::fixture::RecordedInspector::default();

        let catalog = resolve_catalog(
            Path::new("/workspace"),
            &visible,
            &[&ProbeResolver],
            &mut inspector,
        );

        assert_eq!(catalog.entries().len(), MANIFESTS_MAX);
        assert!(!catalog.is_degraded());
    }

    #[test]
    fn test_resolve_catalog_drops_the_manifest_past_the_bound_and_reports_it() {
        let visible = probe_manifests(MANIFESTS_MAX + 1);
        let mut inspector = crate::fixture::RecordedInspector::default();

        let catalog = resolve_catalog(
            Path::new("/workspace"),
            &visible,
            &[&ProbeResolver],
            &mut inspector,
        );

        assert_eq!(catalog.entries().len(), MANIFESTS_MAX);
        assert_eq!(catalog.inputs().count(), MANIFESTS_MAX);
        assert_eq!(
            catalog.degradations(),
            [Degradation {
                resolver: ResolverName::Cargo,
                reason: format!(
                    "1 of {} probe.toml manifests were not read: at most {MANIFESTS_MAX} are \
                     read per workspace",
                    MANIFESTS_MAX + 1
                ),
            }]
        );
    }
}
