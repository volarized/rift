//! `lock`: the wait for a lock and the time it stays held, recorded around the lock the
//! caller already has.
//!
//! A wait is an operation, and an operation that waits on another names nothing about the
//! other. The wrapper here records who waited, how long, how the wait ended, and who held
//! the lock meanwhile, and keeps a held lock in the table of operations in flight until its
//! guard drops. It owns no lock of its own: it polls the caller's acquisition future exactly
//! as the caller would, so queue order, fairness, and cancellation stay the wrapped lock's.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::{Pin, pin};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::time::error::Elapsed;

use crate::flight::{LOCK_HELD_SPAN, LOCK_WAIT_SPAN, with_table};
use crate::measurement::monotonic_now;
use crate::metrics::Histogram;

/// `lock.wait.duration`: one wait, by lock, mode, and `error.type` when it ended without
/// the lock.
const LOCK_WAIT_DURATION: Histogram<3> = Histogram::declare(
    "lock.wait.duration",
    &["lock.name", "lock.mode", "error.type"],
);
/// `lock.held.duration`: the time one acquisition kept its lock, by lock and mode.
const LOCK_HELD_DURATION: Histogram<2> =
    Histogram::declare("lock.held.duration", &["lock.name", "lock.mode"]);

/// Starts recording waits for and holds of the lock `name`, a literal from a closed set
/// such as `index.write`, in exclusive mode.
///
/// ```
/// # async fn run() {
/// let writes = tokio::sync::Mutex::new(());
/// let turn = rift_tracing::lock("index.write").acquire(writes.lock()).await;
/// drop(turn);
/// # }
/// ```
pub const fn lock(name: &'static str) -> Lock {
    Lock {
        name,
        mode: LockMode::Exclusive,
    }
}

/// Whether an acquisition excludes every other holder or shares the lock with readers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockMode {
    Exclusive,
    Shared,
}

impl LockMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Exclusive => "exclusive",
            Self::Shared => "shared",
        }
    }
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitOutcome {
    /// The lock was acquired.
    Acquired,
    /// The attempt was refused at once.
    Refused,
    /// The wait ran past its timeout.
    Timeout,
    /// The wait was dropped before it acquired the lock.
    Cancelled,
}

impl WaitOutcome {
    const fn label(self) -> &'static str {
        match self {
            Self::Acquired => "acquired",
            Self::Refused => "refused",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }

    /// The `error.type` of the wait histogram: empty for an acquisition.
    const fn error_type(self) -> &'static str {
        match self {
            Self::Acquired => "",
            other => other.label(),
        }
    }
}

/// One lock, named, in one mode: the start of a recorded acquisition.
#[derive(Clone, Copy, Debug)]
#[must_use = "a lock records nothing until an acquisition runs"]
pub struct Lock {
    name: &'static str,
    mode: LockMode,
}

impl Lock {
    /// Records the acquisition as exclusive: a mutex, a write lock, or a file lock.
    pub const fn exclusive(mut self) -> Self {
        self.mode = LockMode::Exclusive;
        self
    }

    /// Records the acquisition as shared: a read lock.
    pub const fn shared(mut self) -> Self {
        self.mode = LockMode::Shared;
        self
    }

    /// Awaits `acquisition`, such as `mutex.lock()`, and answers its guard inside a
    /// [`Held`].
    ///
    /// The wait starts at the first poll, which is also when the acquisition future first
    /// runs. An acquisition ready at that poll records no wait span; one that is not opens
    /// the `lock.wait` span, naming the waiting operation and the operation that holds the
    /// lock, and closes it with the outcome `acquired`, or `cancelled` when the future is
    /// dropped first.
    ///
    /// # Cancel safety
    ///
    /// As cancel-safe as `acquisition`: dropping the future drops it, which for a Tokio
    /// lock gives up its place in the queue.
    pub const fn acquire<Acquisition: Future>(
        self,
        acquisition: Acquisition,
    ) -> Acquire<Acquisition> {
        Acquire {
            acquisition,
            lock: self,
            started: None,
            wait: None,
            ending: WaitOutcome::Cancelled,
        }
    }

    /// Awaits `acquisition` for at most `timeout`; a wait past it ends with the outcome
    /// `timeout` and answers [`Elapsed`].
    ///
    /// # Errors
    ///
    /// Returns [`Elapsed`] when `timeout` passes before the lock is acquired.
    ///
    /// # Cancel safety
    ///
    /// As cancel-safe as `acquisition`; the timeout drops it.
    pub async fn acquire_within<Acquisition: Future>(
        self,
        timeout: Duration,
        acquisition: Acquisition,
    ) -> Result<Held<Acquisition::Output>, Elapsed> {
        let mut wait = pin!(self.acquire(acquisition));
        match tokio::time::timeout(timeout, wait.as_mut()).await {
            Ok(held) => Ok(held),
            Err(elapsed) => {
                *wait.as_mut().project().ending = WaitOutcome::Timeout;
                Err(elapsed)
            }
        }
    }

    /// Runs one synchronous `attempt`, such as `file.try_lock()`, and answers its guard
    /// inside a [`Held`].
    ///
    /// A refused attempt records the `lock.wait` span with the outcome `refused`, naming
    /// the operation that holds the lock when this process holds it.
    ///
    /// # Errors
    ///
    /// Returns the attempt's own error when it refuses.
    pub fn try_acquire<Guard, Refusal>(
        self,
        attempt: impl FnOnce() -> Result<Guard, Refusal>,
    ) -> Result<Held<Guard>, Refusal> {
        let started = monotonic_now();
        match attempt() {
            Ok(guard) => {
                self.record_wait(started, WaitOutcome::Acquired);
                Ok(Held::acquired(self, guard, false))
            }
            Err(refusal) => {
                let span = self.wait_span();
                span.record("outcome", WaitOutcome::Refused.label());
                self.record_wait(started, WaitOutcome::Refused);
                Err(refusal)
            }
        }
    }

    /// Opens the `lock.wait` span under the current span, naming the waiter and the holder.
    fn wait_span(self) -> tracing::Span {
        let waiter = current_operation();
        let holder = with_table(|table| table.holder_of(self.name)).flatten();
        tracing::info_span!(
            target: "rift_tracing::lock",
            LOCK_WAIT_SPAN,
            lock.name = self.name,
            lock.mode = self.mode.label(),
            waiter,
            holder,
            outcome = tracing::field::Empty,
        )
    }

    /// Records one wait that started at `started` into `lock.wait.duration`.
    fn record_wait(self, started: Duration, outcome: WaitOutcome) {
        let waited = monotonic_now().saturating_sub(started);
        LOCK_WAIT_DURATION
            .labeled([self.name, self.mode.label(), outcome.error_type()])
            .record(waited);
    }
}

/// The name of the span the calling code runs in, when it runs in one.
fn current_operation() -> Option<&'static str> {
    tracing::Span::current()
        .metadata()
        .map(tracing::Metadata::name)
}

pin_project_lite::pin_project! {
    /// The future [`Lock::acquire`] returns: the caller's acquisition, recorded.
    #[must_use = "a lock is acquired only when the future is awaited"]
    pub struct Acquire<Acquisition> {
        #[pin]
        acquisition: Acquisition,
        lock: Lock,
        started: Option<Duration>,
        wait: Option<tracing::Span>,
        ending: WaitOutcome,
    }

    impl<Acquisition> PinnedDrop for Acquire<Acquisition> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let Some(started) = this.started.take() else {
                return;
            };
            if let Some(span) = this.wait.take() {
                span.record("outcome", this.ending.label());
            }
            this.lock.record_wait(started, *this.ending);
        }
    }
}

impl<Acquisition: Future> Future for Acquire<Acquisition> {
    type Output = Held<Acquisition::Output>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let started = *this.started.get_or_insert_with(monotonic_now);
        match this.acquisition.poll(context) {
            Poll::Ready(guard) => {
                *this.started = None;
                let contended = this.wait.take().is_some_and(|span| {
                    span.record("outcome", WaitOutcome::Acquired.label());
                    true
                });
                this.lock.record_wait(started, WaitOutcome::Acquired);
                Poll::Ready(Held::acquired(*this.lock, guard, contended))
            }
            Poll::Pending => {
                if this.wait.is_none() {
                    *this.wait = Some(this.lock.wait_span());
                }
                Poll::Pending
            }
        }
    }
}

/// A held lock: the wrapped guard, kept in the table of operations in flight until it
/// drops.
///
/// It dereferences to the guard the acquisition answered. Dropping it drops that guard,
/// then closes the `lock.held` span and records the time held, wall time including any
/// suspension it was held across. The drop never blocks, awaits, or panics.
#[must_use = "dropping a held lock releases it at once"]
pub struct Held<Guard> {
    guard: Guard,
    span: tracing::Span,
    lock: Lock,
    acquired: Duration,
}

impl<Guard> Held<Guard> {
    /// Opens the `lock.held` span under the current span: at `INFO` when the acquisition
    /// had to wait, so a contended hold reaches the store beside its wait, and at `DEBUG`
    /// otherwise.
    fn acquired(lock: Lock, guard: Guard, contended: bool) -> Self {
        let holder = current_operation();
        let span = if contended {
            tracing::info_span!(
                target: "rift_tracing::lock",
                LOCK_HELD_SPAN,
                lock.name = lock.name,
                lock.mode = lock.mode.label(),
                holder,
            )
        } else {
            tracing::debug_span!(
                target: "rift_tracing::lock",
                LOCK_HELD_SPAN,
                lock.name = lock.name,
                lock.mode = lock.mode.label(),
                holder,
            )
        };
        Self {
            guard,
            span,
            lock,
            acquired: monotonic_now(),
        }
    }
}

impl<Guard> Deref for Held<Guard> {
    type Target = Guard;

    fn deref(&self) -> &Guard {
        &self.guard
    }
}

impl<Guard> DerefMut for Held<Guard> {
    fn deref_mut(&mut self) -> &mut Guard {
        &mut self.guard
    }
}

impl<Guard> std::fmt::Debug for Held<Guard> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Held")
            .field("lock", &self.lock.name)
            .field("mode", &self.lock.mode.label())
            .finish_non_exhaustive()
    }
}

impl<Guard> Drop for Held<Guard> {
    fn drop(&mut self) {
        let held = monotonic_now().saturating_sub(self.acquired);
        let labels = [self.lock.name, self.lock.mode.label()];
        let recorded = self
            .span
            .with_subscriber(|(_, dispatch)| LOCK_HELD_DURATION.record_in(dispatch, labels, held));
        if recorded != Some(true) {
            LOCK_HELD_DURATION.labeled(labels).record(held);
        }
    }
}

#[cfg(test)]
mod tests;
