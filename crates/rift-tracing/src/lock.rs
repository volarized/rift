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
use std::task::{Context, Poll, ready};
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
        lifelong: false,
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
    /// The attempt was refused, or failed.
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

/// Why an acquisition [`Lock::acquire_fallible`] awaits ended without the lock, carrying
/// the caller's own failure.
#[derive(Debug)]
pub enum Refusal<Failure> {
    /// The lock was refused, or the attempt failed: the wait ends `refused`.
    Refused(Failure),
    /// The wait ran past its budget: the wait ends `timeout`.
    Timeout(Failure),
}

/// One lock, named, in one mode: the start of a recorded acquisition.
#[derive(Clone, Copy, Debug)]
#[must_use = "a lock records nothing until an acquisition runs"]
pub struct Lock {
    name: &'static str,
    mode: LockMode,
    lifelong: bool,
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

    /// Declares the hold lifelong: its holder keeps it for as long as the holder runs, such
    /// as a lock a server takes at start and keeps while it serves.
    ///
    /// The `lock.held` span carries `lifelong = true` and opens as a root, so the operation
    /// that took the lock closes when its work ends; the table of operations in flight
    /// lists the entry with `"lifelong": true` and that operation as its `parent`. The
    /// stall report past `[logs] stall_delay` leaves the entry out, since its age measures
    /// its holder's life rather than stuck work; every other publication of the table lists
    /// it. The wait is recorded as any other.
    pub const fn lifelong(mut self) -> Self {
        self.lifelong = true;
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
    /// The future's output is the guard whatever it holds: an acquisition that can fail,
    /// such as a semaphore's that answers a `Result`, takes [`Self::acquire_fallible`] so
    /// its failure does not record as `acquired`.
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
            wait: Wait::new(self, acquisition),
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
        let mut acquire = pin!(self.acquire(acquisition));
        match tokio::time::timeout(timeout, acquire.as_mut()).await {
            Ok(held) => Ok(held),
            Err(elapsed) => {
                acquire.project().wait.end_as(WaitOutcome::Timeout);
                Err(elapsed)
            }
        }
    }

    /// Awaits `acquisition`, a wait that answers the guard or a [`Refusal`], and answers
    /// the guard inside a [`Held`] or the caller's failure.
    ///
    /// The wait is recorded as [`Self::acquire`] records it, and ends `acquired` with the
    /// guard, `refused` with [`Refusal::Refused`], and `timeout` with
    /// [`Refusal::Timeout`]. A wait that ends without the lock always closes a `lock.wait`
    /// span with that outcome, even when it ended at its first poll, and opens no
    /// `lock.held` span.
    ///
    /// ```
    /// # async fn run() {
    /// use rift_tracing::Refusal;
    ///
    /// let permits = tokio::sync::Semaphore::new(1);
    /// let permit = rift_tracing::lock("worker.permit")
    ///     .acquire_fallible(async { permits.acquire().await.map_err(Refusal::Refused) })
    ///     .await;
    /// assert!(permit.is_ok());
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the failure the [`Refusal`] carries.
    ///
    /// # Cancel safety
    ///
    /// As cancel-safe as `acquisition`: dropping the future drops it, and the wait ends
    /// `cancelled`.
    pub async fn acquire_fallible<Guard, Failure>(
        self,
        acquisition: impl Future<Output = Result<Guard, Refusal<Failure>>>,
    ) -> Result<Held<Guard>, Failure> {
        let (answer, waited) = Wait::new(self, acquisition).await;
        match answer {
            Ok(guard) => {
                let contended = waited.ended(WaitOutcome::Acquired);
                Ok(Held::acquired(self, guard, contended))
            }
            Err(Refusal::Refused(failure)) => {
                waited.ended(WaitOutcome::Refused);
                Err(failure)
            }
            Err(Refusal::Timeout(failure)) => {
                waited.ended(WaitOutcome::Timeout);
                Err(failure)
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
    pub fn try_acquire<Guard, Refused>(
        self,
        attempt: impl FnOnce() -> Result<Guard, Refused>,
    ) -> Result<Held<Guard>, Refused> {
        let waited = Waited {
            lock: self,
            started: monotonic_now(),
            span: None,
        };
        match attempt() {
            Ok(guard) => {
                waited.ended(WaitOutcome::Acquired);
                Ok(Held::acquired(self, guard, false))
            }
            Err(refusal) => {
                waited.ended(WaitOutcome::Refused);
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

/// A wait that ended with the acquisition's answer, not yet recorded.
#[derive(Debug)]
struct Waited {
    lock: Lock,
    started: Duration,
    span: Option<tracing::Span>,
}

impl Waited {
    /// Records the wait's end as `outcome`, and answers whether it had to wait: whether its
    /// `lock.wait` span was open before it ended. An end without the lock opens that span
    /// when no poll opened it, so every refusal and timeout leaves a record.
    fn ended(self, outcome: WaitOutcome) -> bool {
        let contended = self.span.is_some();
        let span = match (outcome, self.span) {
            (WaitOutcome::Acquired, span) => span,
            (_, Some(span)) => Some(span),
            (_, None) => Some(self.lock.wait_span()),
        };
        if let Some(span) = span {
            span.record("outcome", outcome.label());
        }
        self.lock.record_wait(self.started, outcome);
        contended
    }
}

pin_project_lite::pin_project! {
    /// The caller's acquisition, polled as the caller would, with its wait recorded: a
    /// `lock.wait` span from the first poll that finds it pending, and the outcome
    /// `ending` when it drops before it answers.
    struct Wait<Acquisition> {
        #[pin]
        acquisition: Acquisition,
        lock: Lock,
        started: Option<Duration>,
        span: Option<tracing::Span>,
        ending: WaitOutcome,
    }

    impl<Acquisition> PinnedDrop for Wait<Acquisition> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let Some(started) = this.started.take() else {
                return;
            };
            if let Some(span) = this.span.take() {
                span.record("outcome", this.ending.label());
            }
            this.lock.record_wait(started, *this.ending);
        }
    }
}

impl<Acquisition> Wait<Acquisition> {
    const fn new(lock: Lock, acquisition: Acquisition) -> Self {
        Self {
            acquisition,
            lock,
            started: None,
            span: None,
            ending: WaitOutcome::Cancelled,
        }
    }

    /// Records `outcome` in place of `cancelled` when the wait drops before it answers.
    fn end_as(self: Pin<&mut Self>, outcome: WaitOutcome) {
        *self.project().ending = outcome;
    }
}

impl<Acquisition: Future> Future for Wait<Acquisition> {
    type Output = (Acquisition::Output, Waited);

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let started = *this.started.get_or_insert_with(monotonic_now);
        match this.acquisition.poll(context) {
            Poll::Ready(answer) => {
                *this.started = None;
                let waited = Waited {
                    lock: *this.lock,
                    started,
                    span: this.span.take(),
                };
                Poll::Ready((answer, waited))
            }
            Poll::Pending => {
                if this.span.is_none() {
                    *this.span = Some(this.lock.wait_span());
                }
                Poll::Pending
            }
        }
    }
}

pin_project_lite::pin_project! {
    /// The future [`Lock::acquire`] returns: the caller's acquisition, recorded.
    #[must_use = "a lock is acquired only when the future is awaited"]
    pub struct Acquire<Acquisition> {
        #[pin]
        wait: Wait<Acquisition>,
    }
}

impl<Acquisition: Future> Future for Acquire<Acquisition> {
    type Output = Held<Acquisition::Output>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let (guard, waited) = ready!(self.project().wait.poll(context));
        let lock = waited.lock;
        let contended = waited.ended(WaitOutcome::Acquired);
        Poll::Ready(Held::acquired(lock, guard, contended))
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
    /// Declared first: the guard drops, releasing the lock, before the hold is recorded.
    guard: Guard,
    hold: Hold,
}

/// The record of one hold: its `lock.held` span and its start, closed and recorded when
/// it drops.
struct Hold {
    span: tracing::Span,
    lock: Lock,
    acquired: Duration,
}

impl<Guard> Held<Guard> {
    /// Opens the `lock.held` span: at `INFO` when the acquisition had to wait, so a
    /// contended hold reaches the store beside its wait, and at `DEBUG` otherwise.
    ///
    /// The span opens under the current span, except a lifelong hold's: it opens as a root
    /// carrying `lifelong = true`, because a child keeps its parent span open, and the
    /// operation that took the lock would otherwise stay in flight, and close, only when
    /// the hold ends. Its `holder` field and its table entry still name that operation.
    fn acquired(lock: Lock, guard: Guard, contended: bool) -> Self {
        let holder = current_operation();
        let lifelong = lock.lifelong.then_some(true);
        let parent = if lock.lifelong {
            None
        } else {
            tracing::Span::current().id()
        };
        let span = if contended {
            tracing::info_span!(
                target: "rift_tracing::lock",
                parent: parent,
                LOCK_HELD_SPAN,
                lock.name = lock.name,
                lock.mode = lock.mode.label(),
                holder,
                lifelong,
            )
        } else {
            tracing::debug_span!(
                target: "rift_tracing::lock",
                parent: parent,
                LOCK_HELD_SPAN,
                lock.name = lock.name,
                lock.mode = lock.mode.label(),
                holder,
                lifelong,
            )
        };
        Self {
            guard,
            hold: Hold {
                span,
                lock,
                acquired: monotonic_now(),
            },
        }
    }

    /// The same hold over `map(guard)`, such as the file whose lock the guard stands for.
    ///
    /// The `lock.held` span, the table entry, and the start of the hold carry over; the
    /// mapped value drops where the guard would, and the hold is recorded after it.
    pub fn map<Mapped>(self, map: impl FnOnce(Guard) -> Mapped) -> Held<Mapped> {
        Held {
            guard: map(self.guard),
            hold: self.hold,
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
            .field("lock", &self.hold.lock.name)
            .field("mode", &self.hold.lock.mode.label())
            .finish_non_exhaustive()
    }
}

impl Drop for Hold {
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
