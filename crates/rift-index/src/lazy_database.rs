//! A workspace database opened at its first use, not at start.
//!
//! The vectors database exists only for a server whose vector ranking runs: the
//! first vector operation opens it, and with vector ranking disabled nothing does,
//! so no file appears. The handle holds what that open needs - the path and the
//! serving owner - and opens at most once. Concurrent first operations wait for the
//! one open and share its result; an open that fails leaves the handle unopened, and
//! the next operation tries again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rift_error::{RiftError, causes};
use tokio::sync::OnceCell;
use tokio::time::{Instant, timeout_at};

use crate::database::{DatabaseName, DatabasePool, WorkspaceDatabase};

/// A workspace database opened by its first [`LazyDatabase::resolve`].
pub struct LazyDatabase {
    name: DatabaseName,
    path: PathBuf,
    /// The serving owner an open hands its worker. A shutdown drops it, so the handle
    /// itself never outlasts the owner's release.
    owner: Mutex<Option<Arc<dyn Send + Sync>>>,
    opened: OnceCell<Arc<WorkspaceDatabase>>,
    /// Set once a shutdown begins; an open that starts after it refuses.
    closed: AtomicBool,
    #[cfg(test)]
    probe: test_support::OpenProbe,
}

impl std::fmt::Debug for LazyDatabase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LazyDatabase")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("opened", &self.opened.initialized())
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// The refusal a shutdown's own wait answers with when no open is in flight.
struct NeverOpened;

impl LazyDatabase {
    /// A handle on the database `name` at `database_path`; nothing is opened or created.
    ///
    /// `owner` is handed to the worker thread when the database opens, and kept until
    /// that thread exits.
    #[must_use]
    pub fn new(
        database_path: &Path,
        name: DatabaseName,
        owner: Option<Arc<dyn Send + Sync>>,
    ) -> Self {
        Self {
            name,
            path: database_path.to_path_buf(),
            owner: Mutex::new(owner),
            opened: OnceCell::new(),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            probe: test_support::OpenProbe::default(),
        }
    }

    /// Which database this handle opens.
    #[must_use]
    pub const fn name(&self) -> DatabaseName {
        self.name
    }

    /// The database, opened under `pool` when this is the first call.
    ///
    /// The pool is read at the open, never earlier, so the open runs under the bounds
    /// the caller holds at that moment.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the open fails, or when a shutdown began before it.
    /// A failed open emits the `database.open` warning and leaves the handle unopened.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future abandons this attempt; the next call opens again. A worker
    /// that already started keeps its owner until it exits, as
    /// [`WorkspaceDatabase::open_with_owner`] documents.
    pub async fn resolve(&self, pool: DatabasePool) -> Result<Arc<WorkspaceDatabase>, RiftError> {
        self.opened
            .get_or_try_init(|| self.open(pool))
            .await
            .map(Arc::clone)
    }

    /// The database, when an open already succeeded.
    #[must_use]
    pub fn opened(&self) -> Option<Arc<WorkspaceDatabase>> {
        self.opened.get().map(Arc::clone)
    }

    /// Refuses every later open, releases the handle's own share of the owner, and stops
    /// the database by `deadline` when it opened.
    ///
    /// An open in flight finishes first, and the database it opened stops here.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when an open in flight outlasts `deadline`, or the
    /// worker fails or cannot stop before it.
    pub async fn shutdown(&self, deadline: Instant) -> Result<(), RiftError> {
        self.closed.store(true, Ordering::Release);
        drop(
            self.owner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let settled = timeout_at(
            deadline,
            self.opened
                .get_or_try_init(|| async { Err::<Arc<WorkspaceDatabase>, _>(NeverOpened) }),
        )
        .await;
        match settled {
            Ok(Ok(database)) => database.shutdown(deadline).await,
            Ok(Err(NeverOpened)) => Ok(()),
            Err(_elapsed) => Err(self.name.failed(
                &self.path,
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the database's first open outlasted the shutdown deadline",
                ),
            )),
        }
    }

    /// Opens the database once; runs under the cell's initialization turn.
    async fn open(&self, pool: DatabasePool) -> Result<Arc<WorkspaceDatabase>, RiftError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(self.name.failed(
                &self.path,
                std::io::Error::other("the database was shut down before its first open"),
            ));
        }
        #[cfg(test)]
        self.probe.opening().await;
        let owner = self
            .owner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        WorkspaceDatabase::open_with_owner(&self.path, self.name, pool, owner)
            .await
            .inspect_err(|error| {
                let causes = causes(error).join(": ");
                tracing::warn!(
                    component = "storage",
                    operation = "database.open",
                    database = %self.name,
                    path = %self.path.display(),
                    error = %error,
                    causes,
                    "the database failed to open; the next operation that needs it opens it again"
                );
            })
    }
}

#[cfg(test)]
mod test_support {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::oneshot;

    /// Counts the opens a handle starts, and holds the next one at its start.
    #[derive(Default)]
    pub(super) struct OpenProbe {
        pub(super) started: AtomicUsize,
        held: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    }

    impl OpenProbe {
        /// Holds the next open after it starts: the first receiver answers once it
        /// started, and the open continues once the sender is sent to or dropped.
        pub(super) fn hold_next(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (started, started_rx) = oneshot::channel();
            let (release, released) = oneshot::channel();
            *self
                .held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((started, released));
            (started_rx, release)
        }

        pub(super) async fn opening(&self) {
            self.started.fetch_add(1, Ordering::AcqRel);
            let held = self
                .held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some((started, released)) = held {
                let _ = started.send(());
                let _ = released.await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future as _;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use tokio::time::Instant;

    use super::LazyDatabase;
    use crate::{DatabaseName, DatabasePool};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Failure bound on one step; never a way to order two events.
    const STEP_MAX: Duration = Duration::from_secs(10);

    fn pool() -> DatabasePool {
        DatabasePool::new(4, 1_000)
    }

    #[tokio::test]
    async fn a_new_handle_creates_no_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = DatabaseName::Vectors.path(directory.path());
        let handle = LazyDatabase::new(&path, DatabaseName::Vectors, None);

        assert!(handle.opened().is_none());
        handle.shutdown(Instant::now() + STEP_MAX).await?;
        assert!(!path.exists(), "an unused handle creates no database file");
        assert!(
            !DatabaseName::Vectors
                .migration_lock_path(directory.path())
                .exists()
        );
        Ok(())
    }

    /// Two first operations that overlap open the database once and share it.
    #[tokio::test]
    async fn two_concurrent_first_operations_open_the_database_once() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = DatabaseName::Vectors.path(directory.path());
        let handle = Arc::new(LazyDatabase::new(&path, DatabaseName::Vectors, None));
        let (started, release) = handle.probe.hold_next();

        let first_handle = Arc::clone(&handle);
        let first = tokio::spawn(async move { first_handle.resolve(pool()).await });
        tokio::time::timeout(STEP_MAX, started).await??;
        let mut second = Box::pin(handle.resolve(pool()));
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            matches!(second.as_mut().poll(&mut context), Poll::Pending),
            "the second operation waits for the open in flight"
        );
        release.send(()).map_err(|()| "the open dropped its hold")?;

        let first = tokio::time::timeout(STEP_MAX, first).await???;
        let second = tokio::time::timeout(STEP_MAX, second).await??;
        assert!(Arc::ptr_eq(&first, &second), "both share the one open");
        assert_eq!(handle.probe.started.load(Ordering::Acquire), 1);
        assert_eq!(first.name(), DatabaseName::Vectors);
        assert!(path.exists());
        handle.shutdown(Instant::now() + STEP_MAX).await?;
        Ok(())
    }

    /// An open that fails leaves the handle unopened, and the next operation opens it.
    #[tokio::test]
    async fn a_failed_open_is_retried_by_the_next_operation() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = DatabaseName::Vectors.path(directory.path());
        std::fs::create_dir(&path)?;
        let handle = LazyDatabase::new(&path, DatabaseName::Vectors, None);

        let refused = handle
            .resolve(pool())
            .await
            .expect_err("a directory at the database path refuses the open");
        assert_eq!(refused.slug().as_str(), "rift.index.database_failed");
        assert!(handle.opened().is_none());

        std::fs::remove_dir(&path)?;
        let opened = handle.resolve(pool()).await?;
        assert_eq!(handle.probe.started.load(Ordering::Acquire), 2);
        assert!(Arc::ptr_eq(&opened, &handle.opened().ok_or("opened")?));
        handle.shutdown(Instant::now() + STEP_MAX).await?;
        Ok(())
    }

    /// A shutdown refuses every later open, so a stopped server creates no file.
    #[tokio::test]
    async fn an_operation_after_shutdown_opens_nothing() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = DatabaseName::Vectors.path(directory.path());
        let handle = LazyDatabase::new(&path, DatabaseName::Vectors, None);
        handle.shutdown(Instant::now() + STEP_MAX).await?;

        let refused = handle
            .resolve(pool())
            .await
            .expect_err("a closed handle refuses its first open");
        assert_eq!(refused.slug().as_str(), "rift.index.database_failed");
        assert!(!path.exists());
        Ok(())
    }

    /// The worker of a lazily opened database keeps the owner it was handed.
    #[tokio::test]
    async fn the_opened_worker_keeps_the_owner_until_shutdown() -> TestResult {
        let directory = tempfile::tempdir()?;
        let owner: Arc<dyn Send + Sync> = Arc::new(());
        let retained = Arc::downgrade(&owner);
        let handle = LazyDatabase::new(
            &DatabaseName::Vectors.path(directory.path()),
            DatabaseName::Vectors,
            Some(owner),
        );
        let database = handle.resolve(pool()).await?;
        drop(database);
        assert!(retained.upgrade().is_some(), "the worker holds the owner");

        handle.shutdown(Instant::now() + STEP_MAX).await?;
        assert!(
            retained.upgrade().is_none(),
            "a joined worker and the closed handle released the owner"
        );
        drop(handle);
        Ok(())
    }
}
