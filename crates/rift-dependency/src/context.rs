//! The static dependency context: what a workspace's manifests and lockfiles state.
//!
//! [`resolve_context`] runs every shipped resolver's [`DependencyResolver::context`]
//! over one workspace and merges what they read into a [`DependencyContext`]. The pass
//! reads static files alone: it takes a [`StaticInputs`], which offers a file read and
//! nothing else, so no toolchain runs, no package cache is inspected, and no
//! environment value is read. Keep its answer apart from
//! [`DependencyCatalog`](crate::DependencyCatalog), whose entries may carry source
//! roots this machine happens to hold.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rift_protocol::dependencies::{ConfiguredPackage, PackageAvailability, PackageContextEntry};
use rift_protocol::read::ProjectPath;

use crate::catalog::Degradation;
use crate::manifest::claimed_manifests;
use crate::resolver::{
    ContextRequest, DependencyResolver, PACKAGES_MAX, ResolverName, StaticInputs,
};

/// What one resolver read from a workspace's manifests and lockfiles.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextAnswer {
    /// The packages the resolver's manifests and lockfiles state.
    pub entries: Vec<PackageContextEntry>,
    /// The visible workspace paths the resolver read. A change to any of them makes the
    /// answer stale.
    pub inputs: Vec<ProjectPath>,
    /// Everything the resolver could not read, in the order it met each.
    pub degradations: Vec<String>,
}

/// The packages one workspace depends on, as its manifests, lockfiles, and the
/// `[dependencies]` `packages` list state them.
///
/// Entries carry an exact version a lockfile pins or a requirement a manifest declares,
/// never both and never neither. Two entries sharing a package manager, a name, and a
/// selector merge into one, and the first answer's availability stands. Where some input
/// pins an exact version of a package, the requirements declared for that package are
/// dropped: the pin is the stronger answer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DependencyContext {
    entries: Vec<PackageContextEntry>,
    inputs: BTreeSet<ProjectPath>,
    degradations: Vec<Degradation>,
}

impl DependencyContext {
    /// Every package the workspace depends on, in manager, name, then selector order.
    #[must_use]
    pub fn entries(&self) -> &[PackageContextEntry] {
        &self.entries
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

    /// Everything the resolvers could not read, in resolver order.
    #[must_use]
    pub fn degradations(&self) -> &[Degradation] {
        &self.degradations
    }

    /// Whether any input went unread, invalid, or over its bound.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        !self.degradations.is_empty()
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
        self.inputs.extend(answer.inputs);
        self.degradations.extend(
            answer
                .degradations
                .into_iter()
                .map(|reason| Degradation { resolver, reason }),
        );
        if dropped_count > 0 {
            self.degradations.push(Degradation {
                resolver,
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
        DependencyContext {
            entries,
            inputs: self.inputs,
            degradations: self.degradations,
        }
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::dependencies::PackageSelector;
    use rift_protocol::read::Language;

    use super::*;
    use crate::fixture::RecordedInspector;
    use crate::resolver::ResolutionRequest;

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
            PackageAvailability::LocalOnly,
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

        fn manager(&self) -> &'static str {
            "probe"
        }

        fn language(&self) -> Language {
            Language {
                name: "rust".to_owned(),
                dialect: None,
            }
        }

        fn manifest_file_name(&self) -> &'static str {
            "probe.toml"
        }

        fn resolve(
            &self,
            _request: &ResolutionRequest<'_>,
            _inspector: &mut dyn crate::Inspector,
        ) -> crate::Resolution {
            crate::Resolution::default()
        }

        fn context(
            &self,
            request: &ContextRequest<'_>,
            _inputs: &mut dyn StaticInputs,
        ) -> ContextAnswer {
            ContextAnswer {
                entries: self.entries.clone(),
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
                resolver: ResolverName::Cargo,
                reason: "probe.toml: no probe.lock beside it".to_owned(),
            }]
        );
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
                resolver: ResolverName::Cargo,
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
}
