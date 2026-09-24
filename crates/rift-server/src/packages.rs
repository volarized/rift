//! The package branch: the packages a `global` or `all` read answers from.
//!
//! No package is analyzed until a read asks for one. The first read that reaches packages
//! resolves the workspace's package graph once, analyzes the packages the static
//! dependency context names, and holds what it built; every later read reads the same
//! branch. A read whose `scope` stays `local` never reaches this module, so it runs no
//! toolchain and reads no package cache.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rift_dependency::{CatalogEntry, DependencyContext};
use rift_index::{DependencyIndex, DependencyIndexLimits, PackageIndex, package_files};
use rift_protocol::dependencies::PackageContextEntry;
use rift_protocol::read::{PackageIdentity, ProjectPath};

use crate::dependency::{ResolutionPolicy, resolve_workspace_catalog};
use crate::read::{ReadError, ReadFault};

/// The lock a poisoned package branch names.
const PACKAGE_BRANCH_LOCK: &str = "package index";

/// The revision every package publication is built under.
///
/// A package's declarations are analyzed once and never revalidated: the package's bytes
/// are a released artifact, so no package sees a second revision while the server runs.
const PACKAGE_REVISION: u64 = 1;

/// What one fill of the branch produced.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PackageFallback {
    /// Packages this machine analyzed and the branch holds.
    pub(crate) indexed: u64,
    /// Packages the context named that the resolvers found no source for, so nothing was
    /// analyzed for them.
    pub(crate) unresolved: u64,
    /// Exact packages whose source this machine could not resolve.
    pub(crate) unavailable: Vec<PackageIdentity>,
}

/// What one fill needs: where the workspace is, what it makes visible, and which packages
/// its own files name.
#[derive(Clone, Copy)]
pub(crate) struct PackageFill<'request> {
    /// The workspace root the resolvers run over.
    pub(crate) root: &'request Path,
    /// Every visible project path, the set the resolvers read their manifests from.
    pub(crate) visible: &'request [ProjectPath],
    /// How the resolvers reach a package graph, and how long one run may take.
    pub(crate) resolution: ResolutionPolicy,
    /// The bounds one package's analysis and the branch as a whole run under.
    pub(crate) limits: DependencyIndexLimits,
    /// The packages the workspace's manifests and lockfiles name. An empty context
    /// selects nothing, so the fill resolves nothing.
    pub(crate) context: &'request DependencyContext,
}

/// One fill that already ran: what it ran over, and what it produced.
///
/// All three inputs are kept, because each decides the answer. The selection is the
/// packages the dependency context named, so a manifest or lockfile change that adds one
/// refills rather than serving a set that no longer matches the workspace. The bounds are
/// kept because every analyzed package was read under them: an operator who raises
/// `package_files` after a refusal is asking for the refused package. The resolution
/// policy decides how a catalog is reached at all, so turning `resolution` back to `auto`
/// resolves again instead of serving what the static pass alone could find.
#[derive(Clone, Debug)]
struct CompletedFill {
    selection: BTreeSet<(String, String)>,
    limits: DependencyIndexLimits,
    resolution: ResolutionPolicy,
    outcome: PackageFallback,
}

/// The packages this machine analyzed, and the fill that produced them.
///
/// The branch is filled once per set of bounds. Two reads racing for the first fill meet
/// at `fill`, so one resolves and analyzes while the other waits and then reads what it
/// built.
#[derive(Debug)]
pub struct PackageBranch {
    index: RwLock<DependencyIndex>,
    fill: Mutex<Option<CompletedFill>>,
}

impl PackageBranch {
    /// An empty branch, holding no package and not yet filled, under `limits`.
    #[must_use]
    pub fn new(limits: DependencyIndexLimits) -> Self {
        Self {
            index: RwLock::new(DependencyIndex::empty(limits)),
            fill: Mutex::new(None),
        }
    }

    /// A branch already holding `index`, for a test that hands the read service a
    /// prepared package set rather than resolving one. The fill is recorded as done, so
    /// no test reaches a resolver.
    #[cfg(test)]
    pub(crate) fn from_index(index: DependencyIndex) -> Self {
        let indexed = index.indexed_count() as u64;
        let limits = index.limits();
        Self {
            index: RwLock::new(index),
            fill: Mutex::new(Some(CompletedFill {
                selection: BTreeSet::new(),
                limits,
                resolution: ResolutionPolicy::default(),
                outcome: PackageFallback {
                    indexed,
                    unresolved: 0,
                    unavailable: Vec::new(),
                },
            })),
        }
    }

    /// The write side of the branch, taken while one fill inserts what it analyzed.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when a holder panicked with the lock held.
    pub(crate) fn write(&self) -> Result<RwLockWriteGuard<'_, DependencyIndex>, ReadError> {
        self.index
            .write()
            .map_err(|_poisoned| ReadFault::lock_poisoned(PACKAGE_BRANCH_LOCK))
    }

    /// The read side of the branch, held for the life of one answer.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when a holder panicked with the lock held.
    pub(crate) fn read(&self) -> Result<RwLockReadGuard<'_, DependencyIndex>, ReadError> {
        self.index
            .read()
            .map_err(|_poisoned| ReadFault::lock_poisoned(PACKAGE_BRANCH_LOCK))
    }

    /// Fills the branch under `request`'s bounds, and answers what that fill produced.
    ///
    /// A branch already filled over the same selection, under the same bounds and the
    /// same resolution policy, answers what it holds. Any of the three differing from the
    /// recorded ones replaces the branch: every package it holds was analyzed, or
    /// refused, under the old ones.
    ///
    /// An empty package selection performs no resolution, no toolchain run, no package
    /// cache inspection and no analysis: the fill records an empty outcome and returns.
    /// Otherwise the resolvers run once, their catalog is filtered to the packages the
    /// context names, and each matched package's source is analyzed into the branch. A
    /// package whose walk or analysis is refused is recorded as skipped and the fill
    /// carries on, so an incomplete fill answers with what it did build.
    ///
    /// The work is bounded by the context's entry count and by `limits`: one resolver
    /// pass, one walk per matched package, and one analysis per walked package.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when a holder panicked with either lock held.
    pub(crate) fn fill(&self, request: &PackageFill<'_>) -> Result<PackageFallback, ReadError> {
        let mut state = self
            .fill
            .lock()
            .map_err(|_poisoned| ReadFault::lock_poisoned(PACKAGE_BRANCH_LOCK))?;
        let selection = selected_names(request.context);
        if let Some(completed) = state.as_ref()
            && completed.selection == selection
            && completed.limits == request.limits
            && completed.resolution == request.resolution
        {
            return Ok(completed.outcome.clone());
        }
        *self.write()? = DependencyIndex::empty(request.limits);
        let outcome = self.analyze_selected(request, &selection)?;
        *state = Some(CompletedFill {
            selection,
            limits: request.limits,
            resolution: request.resolution,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    /// Resolves, filters, and analyzes the packages one fill selects.
    ///
    /// One record states the fallback the fill ran under, at the end of the pass: no
    /// global package index answered, and these are the counts this machine produced. It
    /// rides one fill rather than one package or one hit.
    fn analyze_selected(
        &self,
        request: &PackageFill<'_>,
        selected: &BTreeSet<(String, String)>,
    ) -> Result<PackageFallback, ReadError> {
        if selected.is_empty() {
            return Ok(PackageFallback::default());
        }
        let span = tracing::info_span!(
            "package.index",
            component = "dependency",
            operation = "package.index",
            selected = selected.len(),
            indexed = tracing::field::Empty,
            skipped = tracing::field::Empty,
        );
        let _entered = span.enter();
        let catalog = resolve_workspace_catalog(request.root, request.visible, request.resolution);
        let entries: Vec<&CatalogEntry> = catalog
            .entries()
            .iter()
            .filter(|entry| selected.contains(&selector(entry.identity())))
            .collect();
        let mut indexed = 0_u64;
        let mut built: Vec<(PackageIdentity, Result<PackageIndex, String>)> =
            Vec::with_capacity(entries.len());
        for entry in &entries {
            if entry.source_root().is_some() {
                let identity = entry.identity().clone();
                let outcome = rift_core::traced!(
                    component = "dependency",
                    operation = "package.analyze",
                    manager = identity.manager.as_str(),
                    name = identity.name.as_str(),
                    { analyzed(entry, &request.limits) }
                );
                built.push((identity, outcome));
            }
        }
        let mut index = self.write()?;
        let mut skipped = 0_usize;
        for (identity, outcome) in built {
            match outcome {
                Ok(package) => match index.insert(package) {
                    Ok(()) => indexed = indexed.saturating_add(1),
                    Err(error) => {
                        index.skip(identity, error.to_string());
                        skipped += 1;
                    }
                },
                Err(reason) => {
                    index.skip(identity, reason);
                    skipped += 1;
                }
            }
        }
        let (unresolved, unavailable) = unavailable_packages(request.context, &entries);
        span.record("indexed", indexed);
        span.record("skipped", skipped);
        tracing::warn!(
            component = "dependency",
            operation = "package.fallback",
            indexed,
            unresolved,
            skipped,
            "no global package index answered; the workspace's packages were analyzed on \
             this machine"
        );
        Ok(PackageFallback {
            indexed,
            unresolved,
            unavailable,
        })
    }
}

fn unavailable_packages(
    context: &DependencyContext,
    entries: &[&CatalogEntry],
) -> (u64, Vec<PackageIdentity>) {
    let resolved: BTreeSet<_> = entries
        .iter()
        .filter(|entry| entry.source_root().is_some())
        .map(|entry| selector(entry.identity()))
        .collect();
    let unresolved: BTreeSet<_> = context
        .entries()
        .iter()
        .map(|entry| (entry.manager.clone(), entry.name.clone()))
        .filter(|key| !resolved.contains(key))
        .collect();
    let mut unavailable = Vec::new();
    for entry in context.entries() {
        let key = (entry.manager.clone(), entry.name.clone());
        if !unresolved.contains(&key) {
            continue;
        }
        if let Some(identity) = entries
            .iter()
            .find(|candidate| selector(candidate.identity()) == key)
            .map(|candidate| candidate.identity().clone())
            .or_else(|| exact_identity(entry))
            && !unavailable.contains(&identity)
        {
            unavailable.push(identity);
        }
    }
    unavailable.sort_by(|left, right| {
        (&left.manager, &left.name, &left.version).cmp(&(
            &right.manager,
            &right.name,
            &right.version,
        ))
    });
    (
        u64::try_from(unresolved.len()).unwrap_or(u64::MAX),
        unavailable,
    )
}

fn exact_identity(entry: &PackageContextEntry) -> Option<PackageIdentity> {
    Some(PackageIdentity {
        manager: entry.manager.clone(),
        name: entry.name.clone(),
        version: entry.version.clone()?,
    })
}

/// One package's analyzed source, or the reason it was refused.
fn analyzed(entry: &CatalogEntry, limits: &DependencyIndexLimits) -> Result<PackageIndex, String> {
    let files = package_files(entry, limits).map_err(|error| error.to_string())?;
    PackageIndex::build(entry, &files, PACKAGE_REVISION).map_err(|error| error.to_string())
}

/// The `(manager, name)` pairs the dependency context names, deduplicated.
///
/// A context entry states an exact version or a declared requirement, and the resolvers
/// state the version they resolved; matching on manager and name alone keeps a package the
/// two spell differently, which a version comparison would drop.
fn selected_names(context: &DependencyContext) -> BTreeSet<(String, String)> {
    context
        .entries()
        .iter()
        .map(|entry: &PackageContextEntry| (entry.manager.clone(), entry.name.clone()))
        .collect()
}

/// One catalog entry's `(manager, name)` selector.
fn selector(identity: &PackageIdentity) -> (String, String) {
    (identity.manager.clone(), identity.name.clone())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use rift_dependency::{DependencyContext, FileObservation, StaticInputs};
    use rift_index::DependencyIndexLimits;
    use rift_protocol::dependencies::{DependenciesConfiguration, DependencyResolution};
    use rift_protocol::read::ProjectPath;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::{PackageBranch, PackageFallback, PackageFill};
    use crate::dependency::ResolutionPolicy;

    /// The span every resolver pass opens. A fill that opens none read no manifest, ran no
    /// toolchain, and inspected no package cache.
    pub(crate) const RESOLVE_SPAN: &str = "dependency.resolve";

    /// Every span opened and every record emitted while a fill ran, in order.
    #[derive(Clone, Debug, Default)]
    pub(crate) struct RecordedSpans {
        spans: Arc<Mutex<Vec<String>>>,
        records: Arc<Mutex<Vec<String>>>,
    }

    impl RecordedSpans {
        /// How many spans named `name` were opened.
        pub(crate) fn named(&self, name: &str) -> usize {
            self.spans
                .lock()
                .expect("the recorder is not poisoned")
                .iter()
                .filter(|recorded| recorded == &name)
                .count()
        }

        /// How many records carry `field` in their rendered fields.
        fn recording(&self, field: &str) -> usize {
            self.records
                .lock()
                .expect("the recorder is not poisoned")
                .iter()
                .filter(|recorded| recorded.contains(field))
                .count()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RecordedSpans {
        fn on_new_span(
            &self,
            attributes: &tracing::span::Attributes<'_>,
            _: &tracing::span::Id,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.spans
                .lock()
                .expect("the recorder is not poisoned")
                .push(attributes.metadata().name().to_owned());
        }

        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Rendered(String);
            impl tracing::field::Visit for Rendered {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write as _;
                    let _ = write!(self.0, " {}={value:?}", field.name());
                }
            }
            let mut rendered = Rendered(event.metadata().level().to_string());
            event.record(&mut rendered);
            self.records
                .lock()
                .expect("the recorder is not poisoned")
                .push(rendered.0);
        }
    }

    /// A workspace whose files this machine does not hold: every read answers absent, so
    /// the resolvers reach a manifest they cannot read.
    struct AbsentInputs;

    impl StaticInputs for AbsentInputs {
        fn read_file(&mut self, _path: &Path, _bytes_max: u64) -> FileObservation {
            FileObservation::Absent
        }
    }

    /// The policy that runs no toolchain, the `resolution = "static"` table's.
    fn static_resolution() -> ResolutionPolicy {
        let configuration = DependenciesConfiguration {
            resolution: DependencyResolution::Static,
            ..DependenciesConfiguration::default()
        };
        ResolutionPolicy::from(&configuration)
    }

    /// A context naming one package no machine holds a source for.
    pub(crate) fn context_naming_one_package() -> DependencyContext {
        context_naming("absent-probe")
    }

    /// A context naming `package` alone, through the operator-configured list: the
    /// resolvers read nothing, so the entry is the whole context.
    fn context_naming(package: &str) -> DependencyContext {
        rift_dependency::resolve_context(
            Path::new("/workspace"),
            &[ProjectPath("Cargo.toml".to_owned())],
            rift_dependency::resolvers(),
            &mut AbsentInputs,
            &[rift_protocol::dependencies::ConfiguredPackage {
                manager: "cargo".to_owned(),
                name: package.to_owned(),
                version: Some("1.0.0".to_owned()),
                requirement: None,
            }],
        )
    }

    /// Fills `branch` under `limits` over `root`, and answers what it produced beside
    /// every span the fill opened.
    fn fill(
        branch: &PackageBranch,
        root: &Path,
        context: &DependencyContext,
        limits: DependencyIndexLimits,
    ) -> (PackageFallback, RecordedSpans) {
        let recorded = RecordedSpans::default();
        let request = PackageFill {
            root,
            visible: &[ProjectPath("Cargo.toml".to_owned())],
            resolution: static_resolution(),
            limits,
            context,
        };
        let outcome = {
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(recorded.clone()),
            );
            branch.fill(&request).expect("the fill holds both locks")
        };
        (outcome, recorded)
    }

    /// An empty context selects no package, so the fill returns before the resolvers run:
    /// no manifest is read, no toolchain runs, and the branch stays empty.
    #[test]
    fn test_an_empty_selection_runs_no_resolver() {
        let root = tempfile::tempdir().expect("a workspace");
        let branch = PackageBranch::new(DependencyIndexLimits::default());

        let (outcome, recorded) = fill(
            &branch,
            root.path(),
            &DependencyContext::default(),
            DependencyIndexLimits::default(),
        );

        assert_eq!(outcome, PackageFallback::default());
        assert_eq!(recorded.named(RESOLVE_SPAN), 0, "{recorded:?}");
        assert_eq!(branch.read().expect("the branch reads").indexed_count(), 0);
    }

    /// A context naming a package the catalog does not carry resolves once and indexes
    /// nothing: the fill analyzes the packages both the context and the catalog name.
    #[test]
    fn test_a_selection_the_catalog_does_not_carry_indexes_nothing() {
        let root = tempfile::tempdir().expect("a workspace");
        let context = context_naming_one_package();
        assert_eq!(context.entries().len(), 1, "{context:?}");
        let branch = PackageBranch::new(DependencyIndexLimits::default());

        let (outcome, recorded) = fill(
            &branch,
            root.path(),
            &context,
            DependencyIndexLimits::default(),
        );

        assert_eq!(outcome.indexed, 0);
        assert_eq!(outcome.unresolved, 1);
        assert_eq!(outcome.unavailable.len(), 1);
        assert_eq!(outcome.unavailable[0].name, "absent-probe");
        assert_eq!(recorded.named(RESOLVE_SPAN), 1, "{recorded:?}");
        assert_eq!(branch.read().expect("the branch reads").indexed_count(), 0);
        assert_eq!(
            recorded.recording("operation=\"package.fallback\""),
            1,
            "one record states the fallback the fill ran under: {recorded:?}"
        );
    }

    /// A manifest or lockfile that names another package changes the selection, so the
    /// next read resolves again rather than serving the set the workspace has left.
    #[test]
    fn test_a_changed_selection_refills_the_branch() {
        let root = tempfile::tempdir().expect("a workspace");
        let branch = PackageBranch::new(DependencyIndexLimits::default());
        let limits = DependencyIndexLimits::default();

        fill(&branch, root.path(), &context_naming_one_package(), limits);
        let (_, again) = fill(
            &branch,
            root.path(),
            &context_naming("another-probe"),
            limits,
        );

        assert_eq!(again.named(RESOLVE_SPAN), 1, "{again:?}");
    }

    /// The branch is filled once per set of bounds: a second read under the same bounds
    /// answers what the first built without resolving again, and bounds that differ
    /// resolve once more.
    #[test]
    fn test_a_fill_repeats_only_under_new_bounds() {
        let root = tempfile::tempdir().expect("a workspace");
        let context = context_naming_one_package();
        let branch = PackageBranch::new(DependencyIndexLimits::default());
        let limits = DependencyIndexLimits::default();

        let (first, _) = fill(&branch, root.path(), &context, limits);
        let (again, repeated) = fill(&branch, root.path(), &context, limits);
        assert_eq!(first, again);
        assert_eq!(repeated.named(RESOLVE_SPAN), 0, "{repeated:?}");
        assert_eq!(
            repeated.recording("operation=\"package.fallback\""),
            0,
            "the fallback is recorded once per fill, not once per read: {repeated:?}"
        );

        let raised = DependencyIndexLimits {
            package_files_max: limits.package_files_max + 1,
            ..limits
        };
        let (_, after) = fill(&branch, root.path(), &context, raised);
        assert_eq!(after.named(RESOLVE_SPAN), 1, "{after:?}");
    }
}
