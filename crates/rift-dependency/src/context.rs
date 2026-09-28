//! The static dependency context: what a workspace's manifests and lockfiles state.
//!
//! [`resolve_context`] runs every shipped resolver's [`DependencyResolver::context`]
//! over one workspace and merges what they read into a [`DependencyContext`]. The pass
//! reads static files alone: it takes a [`StaticInputs`], which offers a file read and a
//! directory listing and nothing else, so no toolchain runs and no environment value is
//! read. The standard library pass adds its entries after, through
//! [`DependencyContext::add_standard_libraries`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rift_protocol::dependencies::{
    ConfiguredPackage, PackageAvailability, PackageContextEntry, RequestedPackage,
};
use rift_protocol::read::{PackageIdentity, ProjectPath};

use crate::manifest::claimed_manifests;
use crate::resolver::{
    ContextRequest, DependencyResolver, PACKAGES_MAX, ResolverName, StaticInputs,
};
use crate::stdlib::{StandardLibrary, StandardLibraryAnswer};

/// One thing a resolver or a standard library probe could not read, or the entries of
/// one package manager a read left out past its entry bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Degradation {
    /// What degraded, as the `resolver` field of a `package_context_degraded` warning
    /// names it.
    pub resolver: Degraded,
    /// What could not be read, and what the context carries instead.
    pub reason: String,
}

/// What one [`Degradation`] names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Degraded {
    /// A resolver, or a standard library entry's probe, read less than the workspace
    /// states.
    Resolver(ResolverName),
    /// A read left out this package manager's entries sorted last, past the entry bound
    /// [`DependencyContext::with_requested`] holds it to. The manager is spelled as the
    /// entries spell it, such as `pypi`.
    Manager(String),
}

impl Degraded {
    /// The spelling a `package_context_degraded` warning carries: the resolver's name,
    /// or the package manager's.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Resolver(resolver) => resolver.as_str(),
            Self::Manager(manager) => manager,
        }
    }
}

impl std::fmt::Display for Degraded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<ResolverName> for Degraded {
    fn from(resolver: ResolverName) -> Self {
        Self::Resolver(resolver)
    }
}

/// Where one package the workspace depends on is installed on this machine.
///
/// The context keeps install folders off the wire: the request to the global API carries
/// the entries alone. A caller maps a path below a folder to the package that owns it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallFolder {
    /// The installed package, at the exact version the lockfile or the probe names.
    pub package: PackageIdentity,
    /// Where the package's files stand.
    pub location: InstallLocation,
}

/// Where one installed package's files stand.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum InstallLocation {
    /// An absolute folder or single-file module: an npm package's `node_modules/<name>`
    /// folder, nested copies included, a Python import folder or module the
    /// distribution's `RECORD` lists, or the Rust standard library below the sysroot.
    Path(PathBuf),
    /// The `<name>-<version>` folder Cargo unpacks a registry package into, below each
    /// registry source folder, `~/.cargo/registry/src/<index>/`. Cargo names the index
    /// folder by a hash of the registry's URL, so the pass mints the folder name from the
    /// lockfile's name and version and reads no package cache to find the index.
    CargoRegistry(String),
}

/// What one resolver read from a workspace's manifests and lockfiles.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextAnswer {
    /// The packages the resolver's manifests and lockfiles state.
    pub entries: Vec<PackageContextEntry>,
    /// Where the pinned packages are installed on this machine, for those the answer
    /// could locate.
    pub install_folders: Vec<InstallFolder>,
    /// The visible workspace paths the resolver read. A change to any of them makes the
    /// answer stale.
    pub inputs: Vec<ProjectPath>,
    /// Everything the resolver could not read, in the order it met each.
    pub degradations: Vec<String>,
}

/// The packages one workspace depends on, as its manifests, lockfiles, the `[dependencies]`
/// `packages` list, and its languages' standard libraries state them.
///
/// Entries carry an exact version a lockfile pins or a requirement a manifest declares,
/// never both and never neither. Two entries sharing a package manager, a name, and a
/// selector merge into one, and the first answer's availability stands. Where some input
/// pins an exact version of a package, the requirements declared for that package are
/// dropped: the pin is the stronger answer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DependencyContext {
    entries: Vec<PackageContextEntry>,
    install_folders: Vec<InstallFolder>,
    inputs: BTreeSet<ProjectPath>,
    degradations: Vec<Degradation>,
    libraries: BTreeSet<StandardLibrary>,
}

impl DependencyContext {
    /// Every package the workspace depends on, in manager, name, then selector order.
    #[must_use]
    pub fn entries(&self) -> &[PackageContextEntry] {
        &self.entries
    }

    /// The entries no public registry serves, in manager, name, then selector order.
    pub fn unavailable_entries(&self) -> impl Iterator<Item = &PackageContextEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.availability != PackageAvailability::Canonical)
    }

    /// Where the packages this context names are installed on this machine, in package
    /// then location order. The request to the global API never carries them.
    pub fn install_folders(&self) -> impl Iterator<Item = &InstallFolder> {
        self.install_folders.iter()
    }

    /// Takes `folders` beside the ones held, keeping each folder once, in package then
    /// location order.
    fn locate(&mut self, folders: impl IntoIterator<Item = InstallFolder>) {
        self.install_folders.extend(folders);
        self.install_folders
            .sort_by(|left, right| install_order(left).cmp(&install_order(right)));
        self.install_folders.dedup();
    }

    /// The visible workspace paths the context was read from, in path order.
    pub fn inputs(&self) -> impl Iterator<Item = &ProjectPath> {
        self.inputs.iter()
    }

    /// Whether a change to `path` makes this context stale.
    #[must_use]
    pub fn depends_on(&self, path: &ProjectPath) -> bool {
        self.inputs.contains(path)
    }

    /// The standard libraries the workspace's languages named when the context was
    /// read. A change that adds a language's first visible path or removes its last
    /// makes the context stale.
    #[must_use]
    pub const fn standard_libraries(&self) -> &BTreeSet<StandardLibrary> {
        &self.libraries
    }

    /// Everything the resolvers and probes could not read, in resolver order.
    #[must_use]
    pub fn degradations(&self) -> &[Degradation] {
        &self.degradations
    }

    /// Whether any input went unread, invalid, or over its bound.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        !self.degradations.is_empty()
    }

    /// This context as one read sends it: every entry held for a package `requested`
    /// names leaves, and each requested package joins as the entry
    /// [`RequestedPackage::context_entry`] builds.
    ///
    /// A requested version therefore replaces every version the workspace pins for that
    /// package, a `path` or `git` entry included, and a package the workspace lacks is
    /// added. Two identical requested entries merge into one.
    ///
    /// The result stays within the smaller of `entries_max` and [`PACKAGES_MAX`]: a
    /// caller passes the entry bound the global API advertises for one resolution request,
    /// or [`PACKAGES_MAX`] before it knows one. Past the bound the held entries sorted last
    /// leave first, the standard library entries among the first, as they are the last the
    /// context itself takes; the requested entries leave only once every held entry has, the
    /// ones sorted last first, since the read names them. Each package manager whose held
    /// entries left gets one [`Degraded::Manager`] degradation counting them, then each
    /// whose requested entries left gets one more. The work is linear in the entries held
    /// plus the ones requested.
    #[must_use]
    pub fn with_requested(&self, requested: &[RequestedPackage], entries_max: usize) -> Self {
        let entries_max = entries_max.min(PACKAGES_MAX);
        let named: BTreeSet<(&str, &str)> = requested
            .iter()
            .map(|package| (package.manager.as_str(), package.name.as_str()))
            .collect();
        let mut added: Vec<PackageContextEntry> = requested
            .iter()
            .map(RequestedPackage::context_entry)
            .collect();
        added.sort();
        added.dedup();
        let mut kept: Vec<PackageContextEntry> = self
            .entries
            .iter()
            .filter(|entry| !named.contains(&(entry.manager.as_str(), entry.name.as_str())))
            .cloned()
            .collect();
        let requested_displaced = added.split_off(entries_max.min(added.len()));
        let room = entries_max - added.len();
        let displaced = kept.split_off(room.min(kept.len()));
        let mut degradations = self.degradations.clone();
        degradations.extend(Displacement::Held.degradations(&kept, &displaced, entries_max));
        degradations.extend(Displacement::Requested.degradations(
            &added,
            &requested_displaced,
            entries_max,
        ));
        let mut entries = kept;
        entries.append(&mut added);
        entries.sort();
        Self {
            entries,
            install_folders: self.install_folders.clone(),
            inputs: self.inputs.clone(),
            degradations,
            libraries: self.libraries.clone(),
        }
    }

    /// Adds the standard library entries, one per package: an entry whose manager and
    /// name the context already holds is left out, so a lockfile's exact `typescript`
    /// wins over the requirement `>=0`, and the service never meets two entries for
    /// one package. Past [`PACKAGES_MAX`] an entry is dropped like any other.
    pub fn add_standard_libraries(&mut self, answer: StandardLibraryAnswer) {
        let held: BTreeSet<(String, String)> = self
            .entries
            .iter()
            .map(|entry| (entry.manager.clone(), entry.name.clone()))
            .collect();
        for entry in answer.entries {
            if held.contains(&(entry.manager.clone(), entry.name.clone()))
                || self.entries.len() >= PACKAGES_MAX
            {
                continue;
            }
            self.entries.push(entry);
        }
        self.entries.sort();
        self.locate(answer.install_folders);
        self.inputs.extend(answer.inputs);
        self.libraries.extend(answer.libraries);
        self.degradations
            .extend(
                answer
                    .degradations
                    .into_iter()
                    .map(|(resolver, reason)| Degradation {
                        resolver: resolver.into(),
                        reason,
                    }),
            );
    }
}

// Under the compiled bound every requested entry stays in `with_requested`, with room
// for the context beside it; only a smaller advertised bound cuts requested entries.
const _: () = assert!(rift_protocol::dependencies::REQUESTED_PACKAGES_MAX < PACKAGES_MAX);

/// Which entries of a read left past its entry bound: the ones the context held, or the
/// ones the request named, which leave only after every held entry has.
#[derive(Clone, Copy, Debug)]
enum Displacement {
    Held,
    Requested,
}

impl Displacement {
    /// One degradation per package manager whose entries `displaced` holds, counting them
    /// against the entries of that manager the read `kept`, in manager order.
    fn degradations(
        self,
        kept: &[PackageContextEntry],
        displaced: &[PackageContextEntry],
        entries_max: usize,
    ) -> Vec<Degradation> {
        let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for entry in displaced {
            counts.entry(entry.manager.as_str()).or_default().1 += 1;
        }
        for entry in kept {
            if let Some((kept_count, _)) = counts.get_mut(entry.manager.as_str()) {
                *kept_count += 1;
            }
        }
        counts
            .into_iter()
            .map(|(manager, (kept_count, dropped_count))| Degradation {
                resolver: Degraded::Manager(manager.to_owned()),
                reason: self.reason(dropped_count, kept_count + dropped_count, entries_max),
            })
            .collect()
    }

    /// Why `dropped_count` of `total_count` entries left under `entries_max`, for a reader.
    fn reason(self, dropped_count: usize, total_count: usize, entries_max: usize) -> String {
        match self {
            Self::Held => format!(
                "{dropped_count} of {total_count} packages were not reported: at most \
                 {entries_max} are carried per read, the requested packages first"
            ),
            Self::Requested => format!(
                "{dropped_count} of {total_count} requested packages were not reported: at \
                 most {entries_max} are carried per read, and every other package left first"
            ),
        }
    }
}

/// Reads the static dependency context of one workspace.
///
/// Each resolver receives the visible paths carrying its manifest file name, at most
/// [`MANIFESTS_MAX`](crate::MANIFESTS_MAX) of them in path order; a workspace with more
/// reports the drop as a degradation. A resolver claiming no visible manifest does not
/// run. `configured` merges first, so an entry the operator names keeps its place when
/// the whole list meets [`PACKAGES_MAX`]; a configured entry stating both selectors or
/// neither contributes nothing, the case configuration acceptance already refuses. The
/// work is proportional to the visible path count plus the bytes each resolver reads.
pub fn resolve_context(
    root: &Path,
    visible: &[ProjectPath],
    resolvers: &[&dyn DependencyResolver],
    inputs: &mut dyn StaticInputs,
    configured: &[ConfiguredPackage],
) -> DependencyContext {
    let mut merge = ContextMerge::default();
    merge.configured(configured);
    for resolver in resolvers {
        let claimed = claimed_manifests(visible, resolver.manifest_file_name());
        if claimed.manifests.is_empty() {
            continue;
        }
        let request = ContextRequest {
            root,
            manifests: &claimed.manifests,
        };
        let mut answer = resolver.context(&request, inputs);
        answer.degradations.extend(claimed.dropped);
        merge.answer(resolver.name(), answer);
    }
    merge.build()
}

/// The order install folders keep: manager, name, version, then location.
fn install_order(folder: &InstallFolder) -> (&str, &str, &str, &InstallLocation) {
    let package = &folder.package;
    (
        &package.manager,
        &package.name,
        &package.version,
        &folder.location,
    )
}

/// The separators that open a version's pre-release and build metadata.
const VERSION_METADATA: [char; 2] = ['-', '+'];
/// The release numbers a whole version states: major, minor, and patch.
const VERSION_NUMBERS: usize = 3;

/// Whether a version states all three release numbers and no wildcard.
///
/// Every ecosystem's pin rests on this: Cargo's `=1.2.3`, npm's bare `1.2.3`, and a PEP
/// 508 `==1.2.3` each name one release only when the version behind the operator is
/// whole.
pub(crate) fn is_whole_version(version: &str) -> bool {
    let release = version.split(VERSION_METADATA).next().unwrap_or(version);
    let numbers: Vec<&str> = release.split('.').collect();
    numbers.len() == VERSION_NUMBERS
        && numbers
            .iter()
            .all(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The fields two context entries merge on: manager, name, and the selector they state.
type EntryKey = (String, String, Option<String>, Option<String>);

/// Every resolver's answer folded into one context, under [`PACKAGES_MAX`].
#[derive(Default)]
struct ContextMerge {
    entries: BTreeMap<EntryKey, PackageContextEntry>,
    install_folders: Vec<InstallFolder>,
    inputs: BTreeSet<ProjectPath>,
    degradations: Vec<Degradation>,
}

impl ContextMerge {
    /// Takes the operator's own list.
    ///
    /// Each configured entry is `canonical`: the operator named a package by manager and
    /// name, which is what a public registry answers for, and the list carries no source
    /// this machine could classify otherwise. An operator naming a package a registry
    /// does not serve states it in the manifest instead, where the resolver reads its
    /// source and classifies it.
    fn configured(&mut self, configured: &[ConfiguredPackage]) {
        for package in configured {
            let Some(selector) = package.selector() else {
                continue;
            };
            self.insert(PackageContextEntry::new(
                &package.manager,
                &package.name,
                selector,
                PackageAvailability::Canonical,
            ));
        }
    }

    /// Takes one resolver's answer, reporting what the entry bound dropped.
    fn answer(&mut self, resolver: ResolverName, answer: ContextAnswer) {
        let mut dropped_count = 0_usize;
        let offered_count = answer.entries.len();
        for entry in answer.entries {
            if !self.insert(entry) {
                dropped_count += 1;
            }
        }
        self.install_folders.extend(answer.install_folders);
        self.inputs.extend(answer.inputs);
        self.degradations
            .extend(answer.degradations.into_iter().map(|reason| Degradation {
                resolver: resolver.into(),
                reason,
            }));
        if dropped_count > 0 {
            self.degradations.push(Degradation {
                resolver: resolver.into(),
                reason: format!(
                    "{dropped_count} of {offered_count} packages were not reported: at most \
                     {PACKAGES_MAX} are carried per workspace"
                ),
            });
        }
    }

    /// Takes one entry, or answers `false` once [`PACKAGES_MAX`] entries stand. An entry
    /// whose key already stands merges into it, and the standing availability wins.
    fn insert(&mut self, entry: PackageContextEntry) -> bool {
        let key = (
            entry.manager.clone(),
            entry.name.clone(),
            entry.version.clone(),
            entry.requirement.clone(),
        );
        if self.entries.contains_key(&key) {
            return true;
        }
        if self.entries.len() >= PACKAGES_MAX {
            return false;
        }
        self.entries.insert(key, entry);
        true
    }

    /// The finished context: entries in manager, name, then selector order, with every
    /// requirement for a package some input pins already dropped.
    fn build(self) -> DependencyContext {
        let pinned: BTreeSet<(String, String)> = self
            .entries
            .values()
            .filter(|entry| entry.version.is_some())
            .map(|entry| (entry.manager.clone(), entry.name.clone()))
            .collect();
        let entries = self
            .entries
            .into_values()
            .filter(|entry| {
                entry.version.is_some()
                    || !pinned.contains(&(entry.manager.clone(), entry.name.clone()))
            })
            .collect();
        let mut context = DependencyContext {
            entries,
            install_folders: Vec::new(),
            inputs: self.inputs,
            degradations: self.degradations,
            libraries: BTreeSet::new(),
        };
        context.locate(self.install_folders);
        context
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::dependencies::PackageSelector;

    use super::*;
    use crate::fixture::RecordedInspector;

    const ROOT: &str = "/workspace";

    fn project(path: &str) -> ProjectPath {
        ProjectPath(path.to_owned())
    }

    fn pinned(name: &str, version: &str) -> PackageContextEntry {
        PackageContextEntry::new(
            "probe",
            name,
            PackageSelector::Version(version.to_owned()),
            PackageAvailability::Canonical,
        )
    }

    fn declared(name: &str, requirement: &str) -> PackageContextEntry {
        PackageContextEntry::new(
            "probe",
            name,
            PackageSelector::Requirement(requirement.to_owned()),
            PackageAvailability::Path,
        )
    }

    fn configured_package(name: &str, version: Option<&str>) -> ConfiguredPackage {
        ConfiguredPackage {
            manager: "probe".to_owned(),
            name: name.to_owned(),
            version: version.map(str::to_owned),
            requirement: None,
        }
    }

    /// A resolver claiming `probe.toml` that answers the entries it was handed.
    #[derive(Debug)]
    struct ProbeResolver {
        entries: Vec<PackageContextEntry>,
        degradations: Vec<String>,
    }

    impl DependencyResolver for ProbeResolver {
        fn name(&self) -> ResolverName {
            ResolverName::Cargo
        }

        fn manifest_file_name(&self) -> &'static str {
            "probe.toml"
        }

        fn context(
            &self,
            request: &ContextRequest<'_>,
            _inputs: &mut dyn StaticInputs,
        ) -> ContextAnswer {
            ContextAnswer {
                entries: self.entries.clone(),
                install_folders: self
                    .entries
                    .iter()
                    .filter_map(|entry| {
                        let version = entry.version.clone()?;
                        Some(InstallFolder {
                            package: PackageIdentity {
                                manager: entry.manager.clone(),
                                name: entry.name.clone(),
                                version,
                            },
                            location: InstallLocation::Path(PathBuf::from(format!(
                                "/installed/{}",
                                entry.name
                            ))),
                        })
                    })
                    .collect(),
                inputs: request.manifests.to_vec(),
                degradations: self.degradations.clone(),
            }
        }
    }

    fn resolve(resolver: &ProbeResolver, configured: &[ConfiguredPackage]) -> DependencyContext {
        let visible = [project("probe.toml"), project("src/lib.rs")];
        let mut inputs = RecordedInspector::default();
        resolve_context(
            Path::new(ROOT),
            &visible,
            &[resolver],
            &mut inputs,
            configured,
        )
    }

    #[test]
    fn test_duplicates_merge_by_manager_name_and_selector_in_deterministic_order() {
        let resolver = ProbeResolver {
            entries: vec![
                pinned("tokio", "1.53.1"),
                pinned("serde", "1.0.228"),
                pinned("tokio", "1.53.1"),
                declared("itoa", "^1"),
                declared("itoa", "^1"),
            ],
            degradations: Vec::new(),
        };

        let context = resolve(&resolver, &[]);

        let spelled: Vec<String> = context
            .entries()
            .iter()
            .map(|entry| {
                let selector = entry
                    .version
                    .as_deref()
                    .or(entry.requirement.as_deref())
                    .unwrap_or_default();
                format!("{}@{selector}", entry.name)
            })
            .collect();
        assert_eq!(spelled, ["itoa@^1", "serde@1.0.228", "tokio@1.53.1"]);
        assert!(!context.is_degraded());
        assert!(context.depends_on(&project("probe.toml")));
        assert!(!context.depends_on(&project("src/lib.rs")));
        assert_eq!(
            context.inputs().collect::<Vec<_>>(),
            [&project("probe.toml")]
        );
    }

    #[test]
    fn test_a_requirement_is_dropped_where_an_input_pins_the_same_package() {
        let resolver = ProbeResolver {
            entries: vec![
                declared("serde", "^1.0"),
                pinned("serde", "1.0.228"),
                declared("itoa", "^1"),
            ],
            degradations: Vec::new(),
        };

        let context = resolve(&resolver, &[]);

        let spelled: Vec<String> = context
            .entries()
            .iter()
            .map(|entry| {
                let selector = entry
                    .version
                    .as_deref()
                    .or(entry.requirement.as_deref())
                    .unwrap_or_default();
                format!("{}@{selector}", entry.name)
            })
            .collect();
        assert_eq!(spelled, ["itoa@^1", "serde@1.0.228"]);
    }

    #[test]
    fn test_configured_entries_merge_with_discovered_ones() {
        let resolver = ProbeResolver {
            entries: vec![pinned("serde", "1.0.228")],
            degradations: Vec::new(),
        };

        let context = resolve(
            &resolver,
            &[
                configured_package("serde", Some("1.0.228")),
                configured_package("itoa", Some("1.0.17")),
                configured_package("no-selector", None),
            ],
        );

        let names: Vec<&str> = context
            .entries()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["itoa", "serde"],
            "a configured entry the discovery also found merges into one, and an entry \
             stating no selector contributes nothing"
        );
        assert!(
            context
                .entries()
                .iter()
                .all(|entry| entry.availability == PackageAvailability::Canonical)
        );
    }

    #[test]
    fn test_a_degradation_names_the_resolver_that_met_it() {
        let resolver = ProbeResolver {
            entries: Vec::new(),
            degradations: vec!["probe.toml: no probe.lock beside it".to_owned()],
        };

        let context = resolve(&resolver, &[]);

        assert!(context.is_degraded());
        assert_eq!(
            context.degradations(),
            [Degradation {
                resolver: ResolverName::Cargo.into(),
                reason: "probe.toml: no probe.lock beside it".to_owned(),
            }]
        );
    }

    #[test]
    fn test_install_folders_merge_off_the_entries_and_unavailable_entries_filter() {
        let resolver = ProbeResolver {
            entries: vec![
                pinned("serde", "1.0.228"),
                pinned("serde", "1.0.228"),
                declared("helper", "^0.1"),
            ],
            degradations: Vec::new(),
        };

        let context = resolve(&resolver, &[]);

        let folders: Vec<(&str, &InstallLocation)> = context
            .install_folders()
            .map(|folder| (folder.package.name.as_str(), &folder.location))
            .collect();
        assert_eq!(
            folders,
            [(
                "serde",
                &InstallLocation::Path(PathBuf::from("/installed/serde"))
            )],
            "one folder per package, and a declared requirement locates nothing"
        );
        let unavailable: Vec<&str> = context
            .unavailable_entries()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(unavailable, ["helper"]);
    }

    /// The configured list and the merged context share one ceiling. An operator who
    /// fills `packages` to its own bound leaves no room for a discovered entry, and the
    /// merge reports every dropped entry rather than dropping it silently.
    #[test]
    fn test_the_configured_bound_and_the_context_bound_are_one_number() {
        assert_eq!(
            PACKAGES_MAX,
            rift_protocol::dependencies::DEPENDENCIES_PACKAGES_MAX
        );
    }

    #[test]
    fn test_entries_past_the_bound_are_dropped_and_reported() {
        let entries: Vec<PackageContextEntry> = (0..=PACKAGES_MAX)
            .map(|index| pinned(&format!("pkg-{index:05}"), "1.0.0"))
            .collect();
        let resolver = ProbeResolver {
            entries,
            degradations: Vec::new(),
        };

        let context = resolve(&resolver, &[configured_package("aaa", Some("0.1.0"))]);

        assert_eq!(context.entries().len(), PACKAGES_MAX);
        assert_eq!(
            context.entries()[0].name,
            "aaa",
            "the configured entry keeps its place"
        );
        assert_eq!(
            context.degradations(),
            [Degradation {
                resolver: ResolverName::Cargo.into(),
                reason: format!(
                    "2 of {} packages were not reported: at most {PACKAGES_MAX} are carried \
                     per workspace",
                    PACKAGES_MAX + 1
                ),
            }]
        );
    }

    #[test]
    fn test_a_resolver_claiming_no_visible_manifest_reads_nothing() {
        let resolver = ProbeResolver {
            entries: vec![pinned("serde", "1.0.228")],
            degradations: Vec::new(),
        };
        let mut inputs = RecordedInspector::default();

        let context = resolve_context(
            Path::new(ROOT),
            &[project("Cargo.toml")],
            &[&resolver],
            &mut inputs,
            &[],
        );

        assert!(context.entries().is_empty());
        assert_eq!(context.inputs().count(), 0);
        assert!(!context.is_degraded());
    }

    #[test]
    fn test_standard_library_entries_leave_a_package_the_lockfile_names() {
        let lockfile_typescript = PackageContextEntry::new(
            "npm",
            "typescript",
            PackageSelector::Version("5.9.3".to_owned()),
            PackageAvailability::Canonical,
        );
        let resolver = ProbeResolver {
            entries: vec![lockfile_typescript.clone()],
            degradations: Vec::new(),
        };
        let mut context = resolve(&resolver, &[]);
        let mut inputs = RecordedInspector::default();
        let answer = crate::stdlib::standard_library_answer(
            &crate::stdlib::StandardLibraryRequest {
                root: Path::new(ROOT),
                libraries: &[crate::StandardLibrary::Node, crate::StandardLibrary::Rust],
                execution: false,
            },
            &mut inputs,
        );

        context.add_standard_libraries(answer.clone());
        context.add_standard_libraries(answer);

        let keys: Vec<(&str, &str, Option<&str>, Option<&str>)> = context
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.manager.as_str(),
                    entry.name.as_str(),
                    entry.version.as_deref(),
                    entry.requirement.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            keys,
            [
                ("npm", "typescript", Some("5.9.3"), None),
                ("stdlib", "node", None, Some(">=0")),
                ("stdlib", "rust", None, Some(">=0")),
            ]
        );
        assert!(context.depends_on(&project("rust-toolchain.toml")));
        assert!(context.depends_on(&project(".nvmrc")));
    }

    fn requested(name: &str, version: Option<&str>) -> RequestedPackage {
        RequestedPackage {
            manager: "probe".to_owned(),
            name: name.to_owned(),
            version: version.map(str::to_owned),
        }
    }

    /// Each entry as `name@selector availability`, in context order.
    fn spelled(context: &DependencyContext) -> Vec<String> {
        context
            .entries()
            .iter()
            .map(|entry| {
                let selector = entry
                    .version
                    .as_deref()
                    .or(entry.requirement.as_deref())
                    .unwrap_or_default();
                format!("{}@{selector} {:?}", entry.name, entry.availability)
            })
            .collect()
    }

    /// A requested version replaces every version the context holds for the package, a
    /// path entry included, and the entries of the packages the request leaves alone
    /// stand as the context read them.
    #[test]
    fn test_a_requested_version_replaces_every_entry_of_its_package() {
        let resolver = ProbeResolver {
            entries: vec![
                pinned("serde", "1.0.228"),
                pinned("serde", "1.0.100"),
                declared("helper", "^0.1"),
                pinned("tokio", "1.53.1"),
            ],
            degradations: vec!["one manifest was unreadable".to_owned()],
        };
        let context = resolve(&resolver, &[]);

        let read = context.with_requested(
            &[
                requested("serde", Some("1.0.200")),
                requested("helper", Some("0.1.4")),
            ],
            PACKAGES_MAX,
        );

        assert_eq!(
            spelled(&read),
            [
                "helper@0.1.4 Canonical",
                "serde@1.0.200 Canonical",
                "tokio@1.53.1 Canonical"
            ]
        );
        assert_eq!(read.unavailable_entries().count(), 0);
        assert_eq!(read.degradations(), context.degradations());
        assert_eq!(
            read.install_folders().collect::<Vec<_>>(),
            context.install_folders().collect::<Vec<_>>()
        );
        assert_eq!(
            spelled(&context),
            [
                "helper@^0.1 Path",
                "serde@1.0.100 Canonical",
                "serde@1.0.228 Canonical",
                "tokio@1.53.1 Canonical"
            ],
            "the snapshot's own context is left as it was"
        );
    }

    /// A package the context lacks joins, an entry without a version asks for `>=0`, and
    /// two identical requested entries merge into one.
    #[test]
    fn test_a_requested_package_the_context_lacks_is_added() {
        let resolver = ProbeResolver {
            entries: vec![pinned("serde", "1.0.228")],
            degradations: Vec::new(),
        };
        let context = resolve(&resolver, &[]);

        let read = context.with_requested(
            &[
                requested("anyhow", None),
                requested("itoa", Some("1.0.17")),
                requested("itoa", Some("1.0.17")),
            ],
            PACKAGES_MAX,
        );

        assert_eq!(
            spelled(&read),
            [
                "anyhow@>=0 Canonical",
                "itoa@1.0.17 Canonical",
                "serde@1.0.228 Canonical"
            ]
        );
    }

    /// Two requested versions of one package both go out, and an empty request leaves
    /// the context as it was.
    #[test]
    fn test_requested_versions_of_one_package_each_go_out() {
        let resolver = ProbeResolver {
            entries: vec![pinned("serde", "1.0.228")],
            degradations: Vec::new(),
        };
        let context = resolve(&resolver, &[]);

        let read = context.with_requested(
            &[
                requested("serde", Some("1.0.200")),
                requested("serde", None),
            ],
            PACKAGES_MAX,
        );

        assert_eq!(
            spelled(&read),
            ["serde@>=0 Canonical", "serde@1.0.200 Canonical"]
        );
        assert_eq!(context.with_requested(&[], PACKAGES_MAX), context);
    }

    /// A context `count` entries short of the bound, held by the probe resolver, with the
    /// standard library entry `stdlib/rust` sorted last.
    fn context_short_of_the_bound_by(count: usize) -> DependencyContext {
        let mut entries: Vec<PackageContextEntry> = (0..PACKAGES_MAX - 1 - count)
            .map(|index| pinned(&format!("pkg-{index:05}"), "1.0.0"))
            .collect();
        entries.push(PackageContextEntry::new(
            "stdlib",
            "rust",
            PackageSelector::Requirement(">=0".to_owned()),
            PackageAvailability::Canonical,
        ));
        resolve(
            &ProbeResolver {
                entries,
                degradations: Vec::new(),
            },
            &[],
        )
    }

    /// Requested packages that fill the context exactly to its bound displace nothing.
    #[test]
    fn test_requested_packages_at_the_bound_displace_nothing() {
        let context = context_short_of_the_bound_by(1);

        let read = context.with_requested(&[requested("anyhow", None)], PACKAGES_MAX);

        assert_eq!(read.entries().len(), PACKAGES_MAX);
        assert!(!read.is_degraded(), "{:?}", read.degradations());
    }

    /// Past the bound the requested packages stay and the held entries sorted last leave,
    /// the standard library entry first, with one degradation per package manager whose
    /// entries left.
    #[test]
    fn test_requested_packages_past_the_bound_displace_the_entries_sorted_last() {
        let context = context_short_of_the_bound_by(0);
        assert_eq!(context.entries().len(), PACKAGES_MAX);

        let read = context.with_requested(
            &[
                requested("anyhow", None),
                requested("pkg-00003", Some("2.0.0")),
                requested("zlib", Some("1.3.1")),
            ],
            PACKAGES_MAX,
        );

        assert_eq!(read.entries().len(), PACKAGES_MAX);
        let spelled = spelled(&read);
        for kept in [
            "anyhow@>=0 Canonical",
            "pkg-00003@2.0.0 Canonical",
            "zlib@1.3.1 Canonical",
        ] {
            assert!(spelled.iter().any(|entry| entry == kept), "{kept}");
        }
        let last_held = format!("pkg-{:05}", PACKAGES_MAX - 2);
        assert!(
            read.entries()
                .iter()
                .all(|entry| entry.manager != "stdlib" && entry.name != last_held),
            "the standard library entry and the last probe entry leave"
        );
        assert_eq!(
            read.degradations(),
            [
                Degradation {
                    resolver: Degraded::Manager("probe".to_owned()),
                    reason: format!(
                        "1 of {} packages were not reported: at most {PACKAGES_MAX} are \
                         carried per read, the requested packages first",
                        PACKAGES_MAX - 2
                    ),
                },
                Degradation {
                    resolver: Degraded::Manager("stdlib".to_owned()),
                    reason: format!(
                        "1 of 1 packages were not reported: at most {PACKAGES_MAX} are \
                         carried per read, the requested packages first"
                    ),
                },
            ]
        );
        assert!(
            !context.is_degraded(),
            "the snapshot's own context is left as it was"
        );
    }

    /// A bound below the context's own, such as the one the global API advertises, cuts the
    /// held entries sorted last with or without requested packages, and the requested
    /// entries stay.
    #[test]
    fn test_a_smaller_entry_bound_displaces_the_entries_sorted_last() {
        let resolver = ProbeResolver {
            entries: ["alpha", "beta", "gamma", "delta"]
                .into_iter()
                .map(|name| pinned(name, "1.0.0"))
                .collect(),
            degradations: Vec::new(),
        };
        let context = resolve(&resolver, &[]);

        let read = context.with_requested(&[requested("zlib", Some("1.3.1"))], 3);
        assert_eq!(
            spelled(&read),
            [
                "alpha@1.0.0 Canonical",
                "beta@1.0.0 Canonical",
                "zlib@1.3.1 Canonical"
            ]
        );
        assert_eq!(
            read.degradations(),
            [Degradation {
                resolver: Degraded::Manager("probe".to_owned()),
                reason: "2 of 4 packages were not reported: at most 3 are carried per read, \
                         the requested packages first"
                    .to_owned(),
            }]
        );

        let unrequested = context.with_requested(&[], 3);
        assert_eq!(
            spelled(&unrequested),
            [
                "alpha@1.0.0 Canonical",
                "beta@1.0.0 Canonical",
                "delta@1.0.0 Canonical"
            ]
        );
        assert_eq!(unrequested.degradations().len(), 1);
        assert_eq!(
            context.with_requested(&[], PACKAGES_MAX + 1),
            context,
            "a bound past the context's own cuts nothing"
        );
    }

    /// A bound below the requested count leaves out every held entry first, then the
    /// requested entries sorted last, and names both counts against the bound.
    #[test]
    fn test_a_bound_below_the_requested_count_cuts_the_requested_entries_sorted_last() {
        let resolver = ProbeResolver {
            entries: vec![pinned("alpha", "1.0.0"), pinned("beta", "1.0.0")],
            degradations: Vec::new(),
        };
        let context = resolve(&resolver, &[]);

        let read = context.with_requested(
            &[
                requested("zlib", Some("1.3.1")),
                requested("anyhow", None),
                requested("itoa", Some("1.0.17")),
            ],
            2,
        );

        assert_eq!(
            spelled(&read),
            ["anyhow@>=0 Canonical", "itoa@1.0.17 Canonical"]
        );
        assert_eq!(
            read.degradations(),
            [
                Degradation {
                    resolver: Degraded::Manager("probe".to_owned()),
                    reason: "2 of 2 packages were not reported: at most 2 are carried per read, \
                             the requested packages first"
                        .to_owned(),
                },
                Degradation {
                    resolver: Degraded::Manager("probe".to_owned()),
                    reason: "1 of 3 requested packages were not reported: at most 2 are carried \
                             per read, and every other package left first"
                        .to_owned(),
                },
            ]
        );
    }

    /// A degradation spells a resolver by its name and a displaced package manager as the
    /// entries spell it.
    #[test]
    fn test_degraded_spells_the_resolver_or_the_manager() {
        assert_eq!(
            Degraded::from(ResolverName::StdlibRust).as_str(),
            "stdlib/rust"
        );
        assert_eq!(Degraded::Manager("pypi".to_owned()).to_string(), "pypi");
    }
}
