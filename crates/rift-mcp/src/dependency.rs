//! The dependency lane: one long-lived task that indexes the packages a publication's
//! catalog names, behind the answers, and the handle a publication hands that catalog to.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Arc;

use rift_dependency::{CatalogEntry, DependencyCatalog};
use rift_index::{DependencyIndexLimits, PackageIndex, PackageIndexError, package_files};
use rift_protocol::read::PackageIdentity;
use rift_server::{DependencyStore, ReadError};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::server::BlockingExecutor;

/// The revision every package index is built under. A package's declarations are built
/// once and never revalidated, so no package sees a second revision.
const PACKAGE_INDEX_REVISION: u64 = 1;

/// The operation name a package build queues on the worker pool under.
const PACKAGE_INDEX_OPERATION: &str = "dependency package index";

/// The dependency lane: one long-lived task owning every package build, and the handle a
/// publication hands its catalog to.
///
/// A publication resolves the catalog; the lane indexes what the catalog names, one
/// package at a time on the worker pool, so no request and no publication awaits a
/// package build. Requests coalesce the way the population lane's do: the channel holds
/// one catalog, a request landing while an earlier one waits overwrites it, and a pass
/// checks between packages whether a newer catalog arrived and restarts over that one.
///
/// A lookup answered while a pass runs says so: the store reports the packages still
/// pending, and the answer carries them as `dependency_index_pending`.
#[derive(Clone, Debug)]
pub(crate) struct DependencyLane {
    catalogs: Arc<watch::Sender<Option<Arc<DependencyCatalog>>>>,
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
        limits: DependencyIndexLimits,
        blocking: BlockingExecutor,
        cancellation: CancellationToken,
    ) -> Self {
        let (catalogs, mut requests) = watch::channel::<Option<Arc<DependencyCatalog>>>(None);
        tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    () = cancellation.cancelled() => return,
                    received = requests.changed() => received,
                };
                if received.is_err() {
                    return;
                }
                let requested = requests.borrow_and_update().clone();
                let Some(catalog) = requested else {
                    continue;
                };
                let pass = IndexPass {
                    store: &store,
                    catalog: &catalog,
                    limits,
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
            catalogs: Arc::new(catalogs),
        }
    }

    /// Hands `catalog` to the lane and returns, never awaiting the pass it asks for.
    ///
    /// A closed channel is a server already shutting down: the lane's task ended with the
    /// cancellation token, and no later lookup will read the packages this pass would have
    /// built. That is a debug line rather than a caller's failure, because the publication
    /// this catalog came from already landed.
    pub(crate) fn request(&self, catalog: Arc<DependencyCatalog>) {
        if self.catalogs.send(Some(catalog)).is_err() {
            tracing::debug!(
                component = "dependency",
                operation = "dependency.index",
                "the dependency lane has ended, so this catalog is not indexed"
            );
        }
    }

    /// Whether the lane's task has ended, which a cancelled token causes.
    ///
    /// The task holds the channel's only receiver, so releasing it is the one observable
    /// end of the lane.
    #[cfg(test)]
    pub(crate) fn has_ended(&self) -> bool {
        self.catalogs.receiver_count() == 0
    }

    /// A lane over `store` on an isolated executor under a token nobody cancels: the
    /// task ends when the last handle drops and closes the channel.
    #[cfg(test)]
    pub(crate) fn spawn_isolated(store: &Arc<DependencyStore>) -> Self {
        Self::spawn(
            Arc::clone(store),
            DependencyIndexLimits::default(),
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

/// One pass over one catalog: the store follows the catalog, then each pending package is
/// built in pass order.
struct IndexPass<'a> {
    store: &'a DependencyStore,
    catalog: &'a DependencyCatalog,
    limits: DependencyIndexLimits,
    blocking: &'a BlockingExecutor,
    requests: &'a watch::Receiver<Option<Arc<DependencyCatalog>>>,
}

impl IndexPass<'_> {
    /// Follows the catalog, then builds each pending package until none is left.
    ///
    /// Every iteration takes the store's next pending package and removes it from the
    /// pending list through `insert` or `skip`, so the loop runs at most `pending_count()`
    /// times. It ends early when a newer catalog arrived, when the worker pool refused a
    /// build, or when the store cannot be reached; the packages still pending wait for the
    /// next request.
    async fn run(self) {
        let entries: BTreeMap<IdentityKey<'_>, &CatalogEntry> = self
            .catalog
            .entries()
            .iter()
            .map(|entry| (identity_key(entry.identity()), entry))
            .collect();
        if self.follow_catalog().is_break() {
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
                // `retain_catalog` queues pending packages from this very catalog, so a
                // pending identity it does not name cannot arise; the skip keeps the loop
                // bounded all the same.
                None => self.skip(identity, "the catalog no longer names the package"),
            };
            if step.is_break() {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    /// Drops the packages the catalog no longer lists and queues the arrivals.
    fn follow_catalog(&self) -> ControlFlow<()> {
        match self.store.write() {
            Ok(mut index) => {
                index.retain_catalog(self.catalog);
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
    async fn index_package(&self, entry: &CatalogEntry) -> ControlFlow<()> {
        let identity = entry.identity().clone();
        let span = tracing::info_span!(
            "dependency.index",
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
            let limits = self.limits;
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

/// A store planned over an empty catalog: what a test that exercises the wiring
/// without packages attaches to its read services.
#[cfg(test)]
pub(crate) fn empty_dependency_store() -> Arc<DependencyStore> {
    use rift_index::DependencyIndex;

    Arc::new(DependencyStore::new(DependencyIndex::planned(
        &DependencyCatalog::default(),
        DependencyIndexLimits::default(),
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
    use rift_index::{DependencyIndex, DependencyIndexLimits};
    use rift_protocol::read::{Language, PackageIdentity};
    use rift_server::DependencyStore;
    use tokio_util::sync::CancellationToken;

    use super::{DependencyLane, empty_dependency_store};
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

    fn spawned(store: &Arc<DependencyStore>) -> (DependencyLane, CancellationToken) {
        let cancellation = CancellationToken::new();
        let lane = DependencyLane::spawn(
            Arc::clone(store),
            DependencyIndexLimits::default(),
            BlockingExecutor::isolated(1, 60_000),
            cancellation.clone(),
        );
        (lane, cancellation)
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

    #[tokio::test]
    async fn the_lane_indexes_a_rooted_package() -> TestResult {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("src"))?;
        fs::write(
            root.path().join("src/lib.rs"),
            "pub fn helper_beacon() {}\nfn helper_private() {}\n",
        )?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);

        lane.request(rooted_helper(root.path()));
        store_within_bound(&store, |index| index.indexed_count() == 1).await?;

        let index = store.read()?;
        assert_eq!(index.pending_count(), 0);
        assert!(index.skipped().is_empty(), "{:?}", index.skipped());
        let package = index.package(&helper()).ok_or("the helper is held")?;
        assert_eq!(package.file_count(), 1);
        assert_eq!(package.entry().identity(), &helper());
        drop(index);

        cancellation.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn a_package_whose_root_is_missing_is_skipped_with_a_reason() -> TestResult {
        let root = tempfile::tempdir()?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);

        lane.request(rooted_helper(&root.path().join("absent")));
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
        fs::create_dir_all(root.path().join("src"))?;
        fs::write(
            root.path().join("src/lib.rs"),
            "pub fn helper_beacon() {}\n",
        )?;
        let store = empty_dependency_store();
        let (lane, cancellation) = spawned(&store);
        lane.request(rooted_helper(root.path()));
        store_within_bound(&store, |index| index.indexed_count() == 1).await?;

        lane.request(catalog(Vec::new()));
        store_within_bound(&store, |index| index.indexed_count() == 0).await?;

        assert_eq!(store.read()?.pending_count(), 0);
        cancellation.cancel();
        Ok(())
    }

    /// A request after the lane's task ended is a shutting-down server, which is a debug
    /// line rather than a caller's failure: the store stays as it was.
    #[tokio::test]
    async fn cancellation_ends_the_task_and_a_later_request_changes_nothing() -> TestResult {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("src"))?;
        fs::write(
            root.path().join("src/lib.rs"),
            "pub fn helper_beacon() {}\n",
        )?;
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

        lane.request(rooted_helper(root.path()));
        tokio::time::sleep(LANE_POLL).await;
        let index = store.read()?;
        assert_eq!(index.indexed_count(), 0);
        assert_eq!(index.pending_count(), 0);
        Ok(())
    }
}
