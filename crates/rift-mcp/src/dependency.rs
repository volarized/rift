//! The dependency lane: one long-lived task that indexes the packages a publication's
//! catalog names, behind the answers, and the handle a publication hands that catalog to.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Arc;

use rift_dependency::{CatalogEntry, DependencyCatalog};
use rift_index::{
    DependencyIndex, DependencyIndexLimits, PackageIndex, PackageIndexError, PackageSelection,
    package_files,
};
use rift_protocol::dependencies::{DEPENDENCIES_ENABLED_DEFAULT, DependenciesConfiguration};
use rift_protocol::read::PackageIdentity;
use rift_server::{DependencyStore, ReadError, ReadFault};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::server::BlockingExecutor;
use crate::validation::PublishedWorkspace;

/// The revision every package index is built under. A package's declarations are built
/// once and never revalidated, so no package sees a second revision.
const PACKAGE_INDEX_REVISION: u64 = 1;

/// The operation name a package build queues on the worker pool under.
const PACKAGE_INDEX_OPERATION: &str = "dependency package index";

/// What the accepted `[dependencies]` table asks of the index: whether it runs, which
/// cataloged packages it plans, and the bounds it reads and holds under.
///
/// A publication compiles one beside its read service, so every request the lane
/// receives carries the plan the same acceptance produced.
#[derive(Clone, Debug)]
pub(crate) struct DependencyPlan {
    /// The bounds every build runs under.
    pub(crate) limits: DependencyIndexLimits,
    /// The `include` and `exclude` globs, compiled.
    pub(crate) selection: PackageSelection,
    /// Whether the index runs at all. `false` empties the store and plans nothing.
    pub(crate) enabled: bool,
}

impl DependencyPlan {
    /// Compiles the accepted table's bounds and selection.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when an `include` or `exclude` pattern is not a valid glob,
    /// the same refusal an invalid `[source]` glob draws.
    pub(crate) fn compile(configuration: &DependenciesConfiguration) -> Result<Self, ReadError> {
        let selection = PackageSelection::compile(configuration).map_err(ReadFault::index)?;
        Ok(Self {
            limits: DependencyIndexLimits::from(configuration),
            selection,
            enabled: configuration.enabled,
        })
    }
}

impl Default for DependencyPlan {
    /// The default `[dependencies]` table's plan.
    fn default() -> Self {
        Self {
            limits: DependencyIndexLimits::default(),
            selection: PackageSelection::default(),
            enabled: DEPENDENCIES_ENABLED_DEFAULT,
        }
    }
}

/// One request the lane indexes: a publication's catalog under its plan.
///
/// A disabled plan carries the empty catalog, so following the request drops every
/// held package and plans nothing until a later publication enables the index again.
#[derive(Clone, Debug)]
pub(crate) struct DependencyRequest {
    catalog: Arc<DependencyCatalog>,
    plan: DependencyPlan,
}

impl DependencyRequest {
    /// The request over `catalog` under `plan`.
    pub(crate) fn new(catalog: Arc<DependencyCatalog>, plan: DependencyPlan) -> Self {
        let catalog = if plan.enabled {
            catalog
        } else {
            Arc::new(DependencyCatalog::default())
        };
        Self { catalog, plan }
    }

    /// The request one publication asks for: its resolved catalog under its plan.
    pub(crate) fn for_publication(published: &PublishedWorkspace) -> Self {
        Self::new(
            Arc::clone(published.reads.dependency_catalog()),
            published.dependency_plan.clone(),
        )
    }

    /// Makes `index` follow this request: a fresh plan under changed bounds or
    /// selection, the catalog alone otherwise.
    pub(crate) fn follow(&self, index: &mut DependencyIndex) {
        index.follow(&self.catalog, self.plan.limits, &self.plan.selection);
    }
}

/// The dependency lane: one long-lived task owning every package build, and the handle a
/// publication hands its catalog to.
///
/// A publication resolves the catalog; the lane indexes what the catalog names, one
/// package at a time on the worker pool, so no request and no publication awaits a
/// package build. Requests coalesce the way the population lane's do: the channel holds
/// one request, a request landing while an earlier one waits overwrites it, and a pass
/// checks between packages whether a newer request arrived and restarts over that one.
/// The channel starts over an empty catalog, seen at creation, so the task waits for
/// the first request.
///
/// A lookup answered while a pass runs says so: the store reports the packages still
/// pending, and the answer carries them as `dependency_index_pending`.
#[derive(Clone, Debug)]
pub(crate) struct DependencyLane {
    requests: Arc<watch::Sender<DependencyRequest>>,
}

impl DependencyLane {
    /// Spawns the lane's task over `store` and returns the handle a publication requests on.
    ///
    /// The task ends when the server does. It races the same cancellation token the index
    /// supervisor runs under, which the last server clone's drop guard cancels.
    ///
    /// # Cancel safety
    ///
    /// Cancelling mid-pass keeps every package already inserted or skipped. A build in
    /// flight on the worker pool runs to its end there and its result is discarded, so
    /// the package stays pending and no lock is held across the cancellation.
    pub(crate) fn spawn(
        store: Arc<DependencyStore>,
        blocking: BlockingExecutor,
        cancellation: CancellationToken,
    ) -> Self {
        let idle = DependencyRequest::new(
            Arc::new(DependencyCatalog::default()),
            DependencyPlan::default(),
        );
        let (sender, mut requests) = watch::channel(idle);
        tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    () = cancellation.cancelled() => return,
                    received = requests.changed() => received,
                };
                if received.is_err() {
                    return;
                }
                let request = requests.borrow_and_update().clone();
                let pass = IndexPass {
                    store: &store,
                    request: &request,
                    blocking: &blocking,
                    requests: &requests,
                };
                tokio::select! {
                    () = cancellation.cancelled() => return,
                    () = pass.run() => {}
                }
            }
        });
        Self {
            requests: Arc::new(sender),
        }
    }

    /// Hands `request` to the lane and returns, never awaiting the pass it asks for.
    ///
    /// A closed channel is a server already shutting down: the lane's task ended with the
    /// cancellation token, and no later lookup will read the packages this pass would have
    /// built. That is a debug line rather than a caller's failure, because the publication
    /// this request came from already landed.
    pub(crate) fn request(&self, request: DependencyRequest) {
        if self.requests.send(request).is_err() {
            tracing::debug!(
                component = "dependency",
                operation = "dependency.index",
                "the dependency lane has ended, so this catalog is not indexed"
            );
        }
    }

    /// Hands `published`'s catalog and plan to the lane.
    pub(crate) fn request_for(&self, published: &PublishedWorkspace) {
        self.request(DependencyRequest::for_publication(published));
    }

    /// Whether the lane's task has ended, which a cancelled token causes.
    ///
    /// The task holds the channel's only receiver, so releasing it is the one observable
    /// end of the lane.
    #[cfg(test)]
    pub(crate) fn has_ended(&self) -> bool {
        self.requests.receiver_count() == 0
    }

    /// A lane over `store` on an isolated executor under a token nobody cancels: the
    /// task ends when the last handle drops and closes the channel.
    #[cfg(test)]
    pub(crate) fn spawn_isolated(store: &Arc<DependencyStore>) -> Self {
        Self::spawn(
            Arc::clone(store),
            BlockingExecutor::isolated(1, 60_000),
            CancellationToken::new(),
        )
    }
}

/// The catalog's identity order, borrowed: manager, then name, then version.
type IdentityKey<'a> = (&'a str, &'a str, &'a str);

fn identity_key(identity: &PackageIdentity) -> IdentityKey<'_> {
    (&identity.manager, &identity.name, &identity.version)
}

/// One pass over one request: the store follows the request, then each pending package
/// is built in pass order.
struct IndexPass<'a> {
    store: &'a DependencyStore,
    request: &'a DependencyRequest,
    blocking: &'a BlockingExecutor,
    requests: &'a watch::Receiver<DependencyRequest>,
}

impl IndexPass<'_> {
    /// Runs one pass under one `dependency.index` span, which records what the pass
    /// left in the store when it closes.
    ///
    /// The span names the pass, not the package: a catalog of three hundred packages
    /// writes one INFO record instead of three hundred, so a `rift://logs` page taken
    /// after a pass still holds the rest of the lane's story. Per-package timing sits
    /// on the `debug_span!` each build opens, which the default `[logs] capture`
    /// filter leaves out.
    async fn run(self) {
        let span = tracing::info_span!(
            "dependency.index",
            component = "dependency",
            operation = "dependency.index",
            packages = self.request.catalog.entries().len(),
            indexed = tracing::field::Empty,
            skipped = tracing::field::Empty,
            deferred = tracing::field::Empty,
            bytes = tracing::field::Empty,
        );
        self.index_catalog().instrument(span.clone()).await;
        self.record_totals(&span);
    }

    /// Follows the request, then builds each pending package until none is left.
    ///
    /// Every iteration takes the store's next pending package and removes it from the
    /// pending list through `insert` or `skip`, so the loop runs at most `pending_count()`
    /// times. It ends early when a newer request arrived, when the worker pool refused a
    /// build, or when the store cannot be reached; the packages still pending wait for the
    /// next request.
    async fn index_catalog(&self) {
        let entries: BTreeMap<IdentityKey<'_>, &CatalogEntry> = self
            .request
            .catalog
            .entries()
            .iter()
            .map(|entry| (identity_key(entry.identity()), entry))
            .collect();
        if self.follow_request().is_break() {
            return;
        }
        loop {
            let identity = match self.next_pending() {
                ControlFlow::Continue(Some(identity)) => identity,
                ControlFlow::Continue(None) | ControlFlow::Break(()) => return,
            };
            if self.requests.has_changed().unwrap_or(true) {
                return;
            }
            let step = match entries.get(&identity_key(&identity)) {
                Some(entry) => self.index_package(entry).await,
                // The store queues pending packages from this very catalog, so a pending
                // identity it does not name cannot arise; the skip keeps the loop bounded
                // all the same.
                None => self.skip(identity, "the catalog no longer names the package"),
            };
            if step.is_break() {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    /// Records what the store holds once the pass ends: the packages it indexed, the
    /// ones a build refused, the ones still pending, and the bytes they hold together.
    ///
    /// A store that cannot be locked records nothing here; `store_unreachable` already
    /// reported the refusal that ended the pass.
    fn record_totals(&self, span: &tracing::Span) {
        let Ok(index) = self.store.read() else {
            return;
        };
        span.record("indexed", index.indexed_count());
        span.record("skipped", index.skipped().len());
        span.record("deferred", index.pending_count());
        span.record("bytes", index.total_bytes());
    }

    /// Makes the store follow the request: a fresh plan under changed bounds or
    /// selection, else the packages the catalog no longer lists dropped and the arrivals
    /// queued.
    fn follow_request(&self) -> ControlFlow<()> {
        match self.store.write() {
            Ok(mut index) => {
                self.request.follow(&mut index);
                ControlFlow::Continue(())
            }
            Err(error) => Self::store_unreachable(&error),
        }
    }

    /// The next package the pass should build, read under the store's read lock.
    fn next_pending(&self) -> ControlFlow<(), Option<PackageIdentity>> {
        match self.store.read() {
            Ok(index) => ControlFlow::Continue(index.next_pending().cloned()),
            Err(error) => Self::store_unreachable(&error),
        }
    }

    /// Builds one package on the worker pool, then inserts it or records its refusal.
    ///
    /// A pool refusal - the queue wait spent, or the build's thread lost - is not a fact
    /// about the package, so it ends the pass and leaves the package pending for the next
    /// request; a build refusal skips the package with the refusal's own text.
    ///
    /// The span is a `debug_span!`, so the pass writes one INFO record rather than one per
    /// package; a reader who wants this package's own timing raises `[logs] capture` to
    /// `debug` in `rift.toml`.
    async fn index_package(&self, entry: &CatalogEntry) -> ControlFlow<()> {
        let identity = entry.identity().clone();
        let span = tracing::debug_span!(
            "dependency.package",
            component = "dependency",
            manager = %identity.manager,
            package = %identity.name,
            version = %identity.version,
            files = tracing::field::Empty,
            bytes = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let built = {
            let entry = entry.clone();
            let limits = self.request.plan.limits;
            self.blocking
                .run(PACKAGE_INDEX_OPERATION, move || {
                    Ok(build_package(&entry, &limits))
                })
                .instrument(span.clone())
                .await
        };
        let _entered = span.enter();
        match built {
            Ok(Ok(package)) => {
                span.record("files", package.file_count());
                span.record("bytes", package.byte_count());
                self.insert(package)
            }
            Ok(Err(error)) => self.skip(identity, &error.to_string()),
            Err(error) => {
                span.record("outcome", "deferred");
                tracing::warn!(
                    component = "dependency",
                    error = %error,
                    "the worker pool refused the package build; the package stays pending \
                     until the next publication"
                );
                ControlFlow::Break(())
            }
        }
    }

    /// Holds one built package, or records the refusal when holding it would cross the
    /// total byte bound.
    fn insert(&self, package: PackageIndex) -> ControlFlow<()> {
        let identity = package.identity().clone();
        let mut index = match self.store.write() {
            Ok(index) => index,
            Err(error) => return Self::store_unreachable(&error),
        };
        match index.insert(package) {
            Ok(()) => {
                tracing::Span::current().record("outcome", "indexed");
                ControlFlow::Continue(())
            }
            Err(error) => {
                let reason = error.to_string();
                index.skip(identity, reason.clone());
                drop(index);
                Self::warn_skipped(&reason);
                ControlFlow::Continue(())
            }
        }
    }

    /// Records that `identity` was refused for `reason`.
    fn skip(&self, identity: PackageIdentity, reason: &str) -> ControlFlow<()> {
        match self.store.write() {
            Ok(mut index) => {
                index.skip(identity, reason.to_owned());
                drop(index);
                Self::warn_skipped(reason);
                ControlFlow::Continue(())
            }
            Err(error) => Self::store_unreachable(&error),
        }
    }

    fn warn_skipped(reason: &str) {
        tracing::Span::current().record("outcome", "skipped");
        tracing::warn!(
            component = "dependency",
            reason = %reason,
            "dependency package skipped"
        );
    }

    /// Ends the pass: a store that cannot be locked answers no lookup either, and the
    /// refusal it reports there is the one to act on.
    fn store_unreachable<Step>(error: &ReadError) -> ControlFlow<(), Step> {
        tracing::warn!(
            component = "dependency",
            operation = "dependency.index",
            error = %error,
            "the dependency store cannot be locked; the pass ends"
        );
        ControlFlow::Break(())
    }
}

/// Reads the files spelling `entry`'s API and parses them into one package index.
fn build_package(
    entry: &CatalogEntry,
    limits: &DependencyIndexLimits,
) -> Result<PackageIndex, PackageIndexError> {
    let files = package_files(entry, limits)?;
    PackageIndex::build(entry, &files, PACKAGE_INDEX_REVISION)
}

/// A store planned over an empty catalog under the default plan: what a test that
/// exercises the wiring without packages attaches to its read services.
#[cfg(test)]
pub(crate) fn empty_dependency_store() -> Arc<DependencyStore> {
    Arc::new(DependencyStore::new(DependencyIndex::planned(
        &DependencyCatalog::default(),
        DependencyIndexLimits::default(),
        PackageSelection::default(),
    )))
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use rift_dependency::{CatalogEntry, DependencyCatalog, Resolution, ResolverName};
    use rift_index::{DependencyIndex, DependencyIndexLimits, LogRecord, PackageSelection};
    use rift_protocol::configuration::LogsConfiguration;
    use rift_protocol::dependencies::{DependenciesConfiguration, PackageNamePattern};
    use rift_protocol::read::{Language, PackageIdentity};
    use rift_server::DependencyStore;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use super::{
        DependencyLane, DependencyPlan, DependencyRequest, IndexPass, empty_dependency_store,
    };
    use crate::server::BlockingExecutor;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    /// Most polls one test spends waiting for the lane's task.
    const LANE_ATTEMPTS_MAX: usize = 400;
    /// Pause between two polls of the store.
    const LANE_POLL: Duration = Duration::from_millis(25);

    fn helper() -> PackageIdentity {
        PackageIdentity {
            manager: "cargo".to_owned(),
            name: "helper".to_owned(),
            version: "0.1.0".to_owned(),
        }
    }

    fn rust() -> Language {
        Language {
            name: "rust".to_owned(),
            dialect: None,
        }
    }

    fn rooted_helper(root: &Path) -> Arc<DependencyCatalog> {
        catalog(vec![CatalogEntry::dependency(
            helper(),
            rust(),
            Some(root.to_path_buf()),
            true,
        )])
    }

    fn catalog(entries: Vec<CatalogEntry>) -> Arc<DependencyCatalog> {
        Arc::new(DependencyCatalog::assemble(vec![(
            ResolverName::Cargo,
            Resolution {
                entries,
                inputs: Vec::new(),
                degradations: Vec::new(),
            },
        )]))
    }

    /// `catalog` under the default plan.
    fn requested(catalog: Arc<DependencyCatalog>) -> DependencyRequest {
        DependencyRequest::new(catalog, DependencyPlan::default())
    }

    /// The default plan under `limits`.
    fn plan_under(limits: DependencyIndexLimits) -> DependencyPlan {
        DependencyPlan {
            limits,
            ..DependencyPlan::default()
        }
    }

    fn spawned(store: &Arc<DependencyStore>) -> (DependencyLane, CancellationToken) {
        let cancellation = CancellationToken::new();
        let lane = DependencyLane::spawn(
            Arc::clone(store),
            BlockingExecutor::isolated(1, 60_000),
            cancellation.clone(),
        );
        (lane, cancellation)
    }

    /// Writes `source` as the helper's `src/lib.rs` below `root`.
    fn write_helper_crate(root: &Path, source: &str) -> TestResult {
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/lib.rs"), source)?;
        Ok(())
    }

    /// One pass over `request`, driven directly: what the lane's task runs per request.
    fn pass<'a>(
        store: &'a DependencyStore,
        request: &'a DependencyRequest,
        blocking: &'a BlockingExecutor,
        requests: &'a watch::Receiver<DependencyRequest>,
    ) -> IndexPass<'a> {
        IndexPass {
            store,
            request,
            blocking,
            requests,
        }
    }

    /// Waits until `settled` holds over the store, or the poll bound is spent.
    async fn store_within_bound(
        store: &DependencyStore,
        settled: impl Fn(&DependencyIndex) -> bool,
    ) -> TestResult {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if settled(&*store.read()?) {
                return Ok(());
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        let index = store.read()?;
        Err(format!(
            "the lane never settled the store: indexed={}, pending={}, skipped={:?}",
            index.indexed_count(),
            index.pending_count(),
            index.skipped()
        )
        .into())
    }

    #[test]
    fn a_plan_compiles_the_table_and_an_invalid_glob_refuses() -> TestResult {
        let table = DependenciesConfiguration {
            enabled: false,
            package_files: 7,
            exclude: vec![PackageNamePattern("cargo/helper".to_owned())],
            ..DependenciesConfiguration::default()
        };
        let plan = DependencyPlan::compile(&table)?;
        assert!(!plan.enabled);
        assert_eq!(plan.limits, DependencyIndexLimits::from(&table));
        assert_eq!(plan.selection, PackageSelection::compile(&table)?);
        assert!(!plan.selection.selects(&helper()));
        let default = DependencyPlan::default();
        assert!(default.enabled);
        assert_eq!(default.limits, DependencyIndexLimits::default());
        assert_eq!(default.selection, PackageSelection::default());

        let broken = DependenciesConfiguration {
            include: vec![PackageNamePattern("cargo/[".to_owned())],
            ..DependenciesConfiguration::default()
        };
        let error = DependencyPlan::compile(&broken).expect_err("an unclosed class");
        assert_eq!(error.descriptor().code(), "configuration_invalid");
        Ok(())
    }

    #[tokio::test]
    async fn the_lane_indexes_a_rooted_package() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(
            root.path(),
            "pub fn helper_beacon() {}\nfn helper_private() {}\n",
        )?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);

        lane.request(requested(rooted_helper(root.path())));
        store_within_bound(&store, |index| index.indexed_count() == 1).await?;

        let index = store.read()?;
        assert_eq!(index.pending_count(), 0);
        assert!(index.skipped().is_empty(), "{:?}", index.skipped());
        let package = index.package(&helper()).ok_or("the helper is held")?;
        assert_eq!(package.file_count(), 1);
        assert_eq!(package.identity(), &helper());
        drop(index);

        cancellation.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn a_package_whose_root_is_missing_is_skipped_with_a_reason() -> TestResult {
        let root = tempfile::tempdir()?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);

        lane.request(requested(rooted_helper(&root.path().join("absent"))));
        store_within_bound(&store, |index| index.skipped().len() == 1).await?;

        let index = store.read()?;
        assert_eq!(index.indexed_count(), 0);
        assert_eq!(index.pending_count(), 0);
        let skipped = &index.skipped()[0];
        assert_eq!(skipped.identity, helper());
        assert!(
            !skipped.reason.is_empty(),
            "a skip carries the refusal's text"
        );
        drop(index);

        cancellation.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn a_later_catalog_without_the_package_drops_it() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);
        lane.request(requested(rooted_helper(root.path())));
        store_within_bound(&store, |index| index.indexed_count() == 1).await?;

        lane.request(requested(catalog(Vec::new())));
        store_within_bound(&store, |index| index.indexed_count() == 0).await?;

        assert_eq!(store.read()?.pending_count(), 0);
        cancellation.cancel();
        Ok(())
    }

    /// A request whose plan turns the index off empties the store, whatever its catalog
    /// names; a later enabled request over the same catalog indexes it again.
    #[tokio::test]
    async fn a_disabled_plan_empties_the_store_and_an_enabled_one_refills_it() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);
        lane.request(requested(rooted_helper(root.path())));
        store_within_bound(&store, |index| index.indexed_count() == 1).await?;

        let disabled = DependencyPlan {
            enabled: false,
            ..DependencyPlan::default()
        };
        lane.request(DependencyRequest::new(rooted_helper(root.path()), disabled));
        store_within_bound(&store, |index| index.indexed_count() == 0).await?;

        assert_eq!(
            store.read()?.pending_count(),
            0,
            "a disabled plan plans nothing"
        );

        lane.request(requested(rooted_helper(root.path())));
        store_within_bound(&store, |index| index.indexed_count() == 1).await?;

        cancellation.cancel();
        Ok(())
    }

    /// A package the plan's selection drops is never planned: not pending, not skipped,
    /// and not held.
    #[tokio::test]
    async fn a_dropped_package_is_never_planned() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);
        let plan = DependencyPlan::compile(&DependenciesConfiguration {
            exclude: vec![PackageNamePattern("cargo/helper".to_owned())],
            ..DependenciesConfiguration::default()
        })?;

        lane.request(DependencyRequest::new(rooted_helper(root.path()), plan));
        store_within_bound(&store, |index| {
            index.selection().exclude() == ["cargo/helper"]
        })
        .await?;

        let index = store.read()?;
        assert_eq!(index.pending_count(), 0);
        assert_eq!(index.indexed_count(), 0);
        assert!(index.skipped().is_empty(), "{:?}", index.skipped());
        drop(index);

        cancellation.cancel();
        Ok(())
    }

    /// A request after the lane's task ended is a shutting-down server, which is a debug
    /// line rather than a caller's failure: the store stays as it was.
    #[tokio::test]
    async fn cancellation_ends_the_task_and_a_later_request_changes_nothing() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);

        cancellation.cancel();
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if lane.has_ended() {
                break;
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        assert!(
            lane.has_ended(),
            "the lane's task must end with the cancellation it races"
        );

        lane.request(requested(rooted_helper(root.path())));
        tokio::time::sleep(LANE_POLL).await;
        let index = store.read()?;
        assert_eq!(index.indexed_count(), 0);
        assert_eq!(index.pending_count(), 0);
        Ok(())
    }

    /// A package the store cannot hold within `total_bytes_max` is skipped with the
    /// refusal's text, and none of it is held.
    #[tokio::test]
    async fn a_package_crossing_the_total_byte_bound_is_skipped_with_the_refusal() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let limits = DependencyIndexLimits {
            total_bytes_max: 0,
            ..DependencyIndexLimits::default()
        };
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);

        lane.request(DependencyRequest::new(
            rooted_helper(root.path()),
            plan_under(limits),
        ));
        store_within_bound(&store, |index| index.skipped().len() == 1).await?;

        let index = store.read()?;
        assert_eq!(
            index.limits(),
            limits,
            "the request's bounds replan the store"
        );
        assert_eq!(index.indexed_count(), 0);
        assert_eq!(index.pending_count(), 0);
        let skipped = &index.skipped()[0];
        assert_eq!(skipped.identity, helper());
        assert!(
            skipped.reason.contains("total_bytes_max=0"),
            "{}",
            skipped.reason
        );
        drop(index);

        cancellation.cancel();
        Ok(())
    }

    /// A request under wider bounds replans the store from nothing, so a package an
    /// earlier bound refused is built again.
    #[tokio::test]
    async fn a_request_under_other_bounds_rebuilds_a_refused_package() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);
        let narrow = DependencyIndexLimits {
            total_bytes_max: 0,
            ..DependencyIndexLimits::default()
        };
        lane.request(DependencyRequest::new(
            rooted_helper(root.path()),
            plan_under(narrow),
        ));
        store_within_bound(&store, |index| index.skipped().len() == 1).await?;

        lane.request(requested(rooted_helper(root.path())));
        store_within_bound(&store, |index| index.indexed_count() == 1).await?;

        let index = store.read()?;
        assert!(index.skipped().is_empty(), "{:?}", index.skipped());
        assert_eq!(index.limits(), DependencyIndexLimits::default());
        drop(index);

        cancellation.cancel();
        Ok(())
    }

    /// A pass over a poisoned store ends before it follows the request, and leaves the
    /// store as the panicking holder left it.
    #[tokio::test]
    async fn a_poisoned_store_ends_the_pass_before_the_catalog_is_followed() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let holder = Arc::clone(&store);
        let panicked = std::thread::spawn(move || {
            let _held = holder.write().expect("the fresh lock is clean");
            panic!("poison the dependency index lock");
        })
        .join();
        assert!(
            panicked.is_err(),
            "the holder must panic with the lock held"
        );
        let request = requested(rooted_helper(root.path()));
        let (_sender, requests) = watch::channel(requested(Arc::new(DependencyCatalog::default())));
        let blocking = BlockingExecutor::isolated(1, 60_000);

        pass(&store, &request, &blocking, &requests).run().await;

        assert!(
            store.read().is_err(),
            "the pass leaves the poisoned store as it found it"
        );
        Ok(())
    }

    /// A worker pool that refuses the build says nothing about the package: the pass ends
    /// with the package still pending, for the next request to build.
    #[tokio::test]
    async fn a_refused_pool_build_leaves_the_package_pending_and_ends_the_pass() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let request = requested(rooted_helper(root.path()));
        let (_sender, requests) = watch::channel(requested(Arc::new(DependencyCatalog::default())));
        let blocking = BlockingExecutor::isolated(1, 60_000);
        blocking.operations.close();

        pass(&store, &request, &blocking, &requests).run().await;

        let index = store.read()?;
        assert_eq!(index.pending_count(), 1);
        assert_eq!(index.indexed_count(), 0);
        assert!(index.skipped().is_empty(), "{:?}", index.skipped());
        Ok(())
    }

    /// A newer request on the channel ends the pass before its next build; the lane's
    /// task restarts over that request with the package still pending.
    #[tokio::test]
    async fn a_newer_request_ends_the_pass_before_the_next_build() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let request = requested(rooted_helper(root.path()));
        let (sender, requests) = watch::channel(request.clone());
        sender.send(requested(Arc::new(DependencyCatalog::default())))?;
        let blocking = BlockingExecutor::isolated(1, 60_000);

        pass(&store, &request, &blocking, &requests).run().await;

        let index = store.read()?;
        assert_eq!(index.pending_count(), 1);
        assert_eq!(index.indexed_count(), 0);
        Ok(())
    }

    /// `count` packages, each named apart and rooted at `root`.
    fn rooted_packages(root: &Path, count: usize) -> Arc<DependencyCatalog> {
        catalog(
            (0..count)
                .map(|index| {
                    CatalogEntry::dependency(
                        PackageIdentity {
                            manager: "cargo".to_owned(),
                            name: format!("helper{index}"),
                            version: "0.1.0".to_owned(),
                        },
                        rust(),
                        Some(root.to_path_buf()),
                        true,
                    )
                })
                .collect(),
        )
    }

    /// Runs one pass with the log sink installed under the default `[logs] capture`
    /// filter, and returns what the sink queued.
    async fn recorded_pass(
        store: &DependencyStore,
        request: &DependencyRequest,
        blocking: &BlockingExecutor,
        requests: &watch::Receiver<DependencyRequest>,
    ) -> TestResult<Vec<LogRecord>> {
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let (sink, mut drain) = crate::logs::log_capture();
        let filter = tracing_subscriber::EnvFilter::try_new(LogsConfiguration::default().capture)?;
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(sink.with_filter(filter)),
        );

        pass(store, request, blocking, requests).run().await;

        drop(guard);
        let mut records = Vec::new();
        while let Ok(record) = drain.try_recv_record() {
            records.push(record);
        }
        Ok(records)
    }

    /// The span-close records a recorded pass wrote.
    fn closed(records: &[LogRecord]) -> Vec<&LogRecord> {
        records
            .iter()
            .filter(|record| record.fields().contains("\"span\":\"closed\""))
            .collect()
    }

    /// One pass writes one closed `dependency.index` record, whatever the catalog's
    /// size, and that record carries what the pass left in the store.
    #[tokio::test]
    async fn a_pass_records_one_closed_dependency_index_span() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let request = requested(rooted_packages(root.path(), 3));
        let (_sender, requests) = watch::channel(requested(Arc::new(DependencyCatalog::default())));
        let blocking = BlockingExecutor::isolated(1, 60_000);

        let records = recorded_pass(&store, &request, &blocking, &requests).await?;

        let closed = closed(&records);
        assert_eq!(closed.len(), 1, "{records:?}");
        assert_eq!(closed[0].message(), "dependency.index");
        assert_eq!(closed[0].component(), "dependency");
        assert_eq!(closed[0].operation(), "dependency.index");
        let fields = closed[0].fields();
        assert!(fields.contains("\"packages\":\"3\""), "{fields}");
        assert!(fields.contains("\"indexed\":\"3\""), "{fields}");
        assert!(fields.contains("\"skipped\":\"0\""), "{fields}");
        assert!(fields.contains("\"deferred\":\"0\""), "{fields}");
        assert!(fields.contains("\"bytes\":\""), "{fields}");
        Ok(())
    }

    /// A package a build refuses keeps its own WARN record, and the pass's span counts
    /// the refusal.
    #[tokio::test]
    async fn a_refused_package_keeps_its_skip_warning() -> TestResult {
        let root = tempfile::tempdir()?;
        let store = empty_dependency_store();
        let request = requested(rooted_helper(&root.path().join("absent")));
        let (_sender, requests) = watch::channel(requested(Arc::new(DependencyCatalog::default())));
        let blocking = BlockingExecutor::isolated(1, 60_000);

        let records = recorded_pass(&store, &request, &blocking, &requests).await?;

        let warned = records
            .iter()
            .find(|record| record.message() == "dependency package skipped")
            .unwrap_or_else(|| panic!("the skip keeps its warning: {records:?}"));
        assert_eq!(warned.level(), "warn");
        assert_eq!(warned.component(), "dependency");
        let closed = closed(&records);
        assert_eq!(closed.len(), 1, "{records:?}");
        assert!(
            closed[0].fields().contains("\"skipped\":\"1\""),
            "{}",
            closed[0].fields()
        );
        Ok(())
    }

    /// The per-package span is a debug span, which the default capture filter leaves
    /// out: a pass over three packages records no per-package line.
    #[tokio::test]
    async fn the_per_package_span_stays_out_under_the_default_capture() -> TestResult {
        let root = tempfile::tempdir()?;
        write_helper_crate(root.path(), "pub fn helper_beacon() {}\n")?;
        let store = empty_dependency_store();
        let request = requested(rooted_packages(root.path(), 3));
        let (_sender, requests) = watch::channel(requested(Arc::new(DependencyCatalog::default())));
        let blocking = BlockingExecutor::isolated(1, 60_000);

        let records = recorded_pass(&store, &request, &blocking, &requests).await?;

        assert!(
            records
                .iter()
                .all(|record| record.message() != "dependency.package"),
            "{records:?}"
        );
        assert!(
            records
                .iter()
                .all(|record| !record.fields().contains("\"package\":")),
            "{records:?}"
        );
        Ok(())
    }
}
