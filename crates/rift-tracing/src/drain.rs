//! The log drain: the task that writes the capture queue into the metrics database.
//!
//! The drain takes what the queue holds in batches and appends each through
//! [`LogStore::append`]. A batch the store refuses stays with the drain, which retries
//! it on its own timer until the store accepts it: no attempt count gives up on a store
//! that is slow rather than gone. While it retries, the queue behind it keeps filling to
//! [`LOG_QUEUE_RECORDS`](crate::LOG_QUEUE_RECORDS) and then drops and counts, so the
//! drain holds at most one batch plus the queue, and the next batch it writes carries
//! the drop count. A stop bounds the retry: [`RunningLogDrain::stop`] aborts a drain
//! that outlasts its deadline and answers the records it accepted and never wrote.
//!
//! A process that serves several workspaces writes no single store. Its drain routes
//! each record by the `workspace` field the record, or a span around it, carries, to the
//! consumer of that workspace: a drain of its own, with a bounded queue of
//! [`LOG_WORKSPACE_QUEUE_RECORDS`], writing that workspace's store. A record that names no
//! workspace with a consumer reaches stderr alone.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use rift_error::{RiftError, causes};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::capture::{LOG_QUEUE_DROPPED, LogSink, QUEUE_FULL, UNWRITTEN, now_ms};
use crate::record::{LOG_BATCH_RECORDS_MAX, LogRecord};
use crate::stderr::StderrBound;
use crate::store::LogStore;
use crate::subscriptions::{LOG_SUBSCRIPTION_BYTES_MAX, RecordBudget};

/// Wall-clock span the drain waits for more records before writing what it holds.
pub(crate) const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
/// Longest a `rift://logs` read waits for the drain to write through the sequence the
/// sink had stamped when the read began. A read past this answers with what the store
/// holds: a log read never fails, and never hangs, because the log drain is slow.
pub const LOG_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Wall-clock span between two attempts at a batch the store refused.
pub(crate) const LOG_WRITE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Accepted delivery limits shared by a capture and its workspace consumers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LogDeliveryOptions {
    pub(crate) queue_records: usize,
    pub(crate) flush_interval: Duration,
    pub(crate) retry_interval: Duration,
    pub(crate) settle_timeout: Duration,
}

impl Default for LogDeliveryOptions {
    fn default() -> Self {
        Self {
            queue_records: crate::LOG_QUEUE_RECORDS,
            flush_interval: LOG_FLUSH_INTERVAL,
            retry_interval: LOG_WRITE_RETRY_INTERVAL,
            settle_timeout: LOG_SETTLE_TIMEOUT,
        }
    }
}
/// Records one workspace consumer's queue holds before a routed record is dropped, and the
/// most records one of its batches carries.
///
/// A consumer holds at most this queue and one batch, so a routing drain holds at most
/// `2 * LOG_WORKSPACE_QUEUE_RECORDS` records per consumer and
/// [`RunningLogDrain::WORKSPACE_CONSUMERS_MAX`] times that in all: 32,768 records, eight
/// times the process queue of [`LOG_QUEUE_RECORDS`](crate::LOG_QUEUE_RECORDS). One
/// workspace that emits more than this between two flushes loses the excess and the
/// others lose nothing.
const LOG_WORKSPACE_QUEUE_RECORDS: usize = 256;
/// Where a record's fields carry the workspace it belongs to, in the order the routing
/// drain reads them: the record's own field, the nearest span's, then the root span's.
const WORKSPACE_FIELD_POINTERS: [&str; 3] = [
    "/workspace",
    "/nearest_span/fields/workspace",
    "/root_span/fields/workspace",
];

/// One record on its way to the drain, under the sequence the sink stamped on it.
///
/// The sequence is what a read waits on. A count cannot serve: a full queue drops the
/// newest record while older ones are still queued, so the number of records the drain
/// has finished with says nothing about which ones.
#[derive(Debug)]
pub(crate) struct QueuedRecord {
    pub(crate) sequence: u64,
    pub(crate) record: LogRecord,
    /// Retained bytes until the batch commits or is discarded; notices carry none.
    pub(crate) bytes: Option<OwnedSemaphorePermit>,
}

/// How far the drain has got through the sequence the sink stamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LaneProgress {
    /// Highest sequence the drain has written. Every lower sequence is written or
    /// dropped, because the queue is first in, first out.
    written_through: u64,
    /// Records the lane is finished with: written by the drain, or dropped by a full
    /// queue.
    finished: u64,
}

/// What the log lane has taken, and how far it has got.
///
/// A `rift://logs` read stamps its target from `accepted` on entry and waits for the
/// drain to write through it, so the read sees the records its own request produced
/// rather than whatever the drain's timer had committed by then. Without it the drain's
/// flush interval is a window in which a caller reads back its own missing diagnostic.
#[derive(Debug)]
pub(crate) struct LogSettlement {
    accepted: AtomicU64,
    progress: watch::Sender<LaneProgress>,
    flush: Notify,
    draining: AtomicBool,
    /// The workspace consumers of a routing drain, set when one starts on this lane.
    routes: OnceLock<Arc<LogRoutes>>,
    options: LogDeliveryOptions,
}

impl Default for LogSettlement {
    fn default() -> Self {
        Self {
            accepted: AtomicU64::new(0),
            progress: watch::Sender::new(LaneProgress::default()),
            flush: Notify::new(),
            draining: AtomicBool::new(false),
            routes: OnceLock::new(),
            options: LogDeliveryOptions::default(),
        }
    }
}

impl LogSettlement {
    pub(crate) fn with_options(options: LogDeliveryOptions) -> Self {
        Self {
            options,
            ..Self::default()
        }
    }

    /// Stamps one record on its way into the queue and answers its sequence. Stamped
    /// before the send, so a read taken immediately after a traced call cannot observe a
    /// sequence that misses it.
    pub(crate) fn accept(&self) -> u64 {
        self.accepted.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// One record the queue had no room for. It is finished with, and no sequence
    /// advances: a record the drain never sees is one no read can wait for.
    pub(crate) fn finish_dropped(&self) {
        self.progress.send_modify(|progress| progress.finished += 1);
    }

    /// One written batch: `count` records finished, the highest sequence among them
    /// written.
    fn finish_written(&self, count: u64, written_through: u64) {
        self.progress.send_modify(|progress| {
            progress.finished += count;
            progress.written_through = progress.written_through.max(written_through);
        });
    }

    /// Whether a read stamped at `target` may proceed.
    ///
    /// Either the drain has written through that sequence, or the lane has finished with
    /// everything it has taken, which says the record at `target` was dropped and no
    /// wait will produce it.
    fn reached(&self, progress: LaneProgress, target: u64) -> bool {
        progress.written_through >= target
            || progress.finished >= self.accepted.load(Ordering::SeqCst)
    }

    /// Waits for the drain to write through the sequence stamped when the wait began.
    ///
    /// Returns at once when no drain is running and when the lane is already there.
    /// Past `deadline` the caller proceeds with whatever the store holds. A routing
    /// drain finishes a record when it hands it to a consumer.
    async fn settle_by(&self, deadline: Instant) {
        if !self.draining.load(Ordering::SeqCst) {
            return;
        }
        let target = self.accepted.load(Ordering::SeqCst);
        let mut progress = self.progress.subscribe();
        if self.reached(*progress.borrow_and_update(), target) {
            return;
        }
        self.flush.notify_one();
        let reached = async {
            while progress.changed().await.is_ok() {
                if self.reached(*progress.borrow_and_update(), target) {
                    return;
                }
            }
        };
        let _ = tokio::time::timeout_at(deadline, reached).await;
    }
}

/// Waits for the calling thread's log lane to write through what it has stamped, and,
/// when the lane routes, for the consumer of `workspace` to write what it was handed.
///
/// Every `rift://logs` read calls this before it opens the store, naming the workspace
/// root it serves as the `workspace` field spells it. The lane is the [`LogSink`] layer
/// of the thread's current `tracing` dispatcher. A process can build more than one lane -
/// under `cargo test` every test is a thread of one process and builds its own - and the
/// dispatcher is what decides where a thread's records go. A process that records
/// nothing - every command but the foreground server - installs no sink, and its log
/// reads wait for nothing. Both waits share the accepted `[logs] settle_timeout`,
/// [`LOG_SETTLE_TIMEOUT`] by default.
///
/// # Cancel safety
///
/// Dropping the future ends the wait and changes nothing.
pub async fn settle_for_read(workspace: &str) {
    let Some(settlement) = installed_settlement() else {
        return;
    };
    let deadline = Instant::now() + settlement.options.settle_timeout;
    settlement.settle_by(deadline).await;
    let consumer = settlement
        .routes
        .get()
        .and_then(|routes| routes.settlement_of(workspace));
    if let Some(consumer) = consumer {
        consumer.settle_by(deadline).await;
    }
}

/// The lane of the [`LogSink`] layer in the calling thread's current dispatcher.
fn installed_settlement() -> Option<Arc<LogSettlement>> {
    tracing::dispatcher::get_default(|dispatch| {
        dispatch
            .downcast_ref::<LogSink>()
            .map(|sink| Arc::clone(&sink.settlement))
    })
}

/// What a stop asks of the log lane once its drain is joined or aborted: how many
/// records it took and never wrote, and, for a process whose standard error is cut at
/// `[logs] stderr_limit`, how many bytes that bound discarded.
///
/// A clone taken before the drain runs keeps answering after the drain task is aborted.
#[derive(Clone, Debug)]
pub struct LogLane {
    dropped: Arc<AtomicU64>,
    settlement: Arc<LogSettlement>,
    stderr: Option<Arc<StderrBound>>,
}

impl LogLane {
    /// Records the lane accepted and has not finished with, plus dropped records whose
    /// count no batch has carried yet.
    ///
    /// After the drain is aborted, this is what the stop lost: the queued records, the
    /// batch the drain held, and the drops its next batch would have reported. Drops a
    /// held batch already carried in its notice are not counted again.
    #[must_use]
    pub fn unwritten(&self) -> u64 {
        self.never_written()
            .saturating_add(self.dropped.load(Ordering::Relaxed))
    }

    /// Records the lane accepted and neither wrote nor dropped at a full queue: after the
    /// drain is aborted, the queued records and the batch it held.
    fn never_written(&self) -> u64 {
        let accepted = self.settlement.accepted.load(Ordering::SeqCst);
        let finished = self.settlement.progress.borrow().finished;
        accepted.saturating_sub(finished)
    }
}

/// The queue's reading end, and the task that writes it into the store.
#[derive(Debug)]
pub struct LogDrain {
    receiver: Receiver<QueuedRecord>,
    dropped: Arc<AtomicU64>,
    settlement: Arc<LogSettlement>,
    stderr: Option<Arc<StderrBound>>,
    /// Most records one write carries: [`LOG_BATCH_RECORDS_MAX`], or
    /// [`LOG_WORKSPACE_QUEUE_RECORDS`] for a workspace consumer.
    batch_records_max: usize,
    budget: RecordBudget,
}

/// Why the next persistence flush may begin.
enum FlushReady {
    /// The flush interval elapsed or a read requested a flush.
    Due,
    /// Cancellation ends collection and closes the receiving end.
    Cancelled,
}

impl LogDrain {
    /// The reading end of the queue [`log_capture`](crate::log_capture) built.
    pub(crate) fn new(
        receiver: Receiver<QueuedRecord>,
        dropped: Arc<AtomicU64>,
        settlement: Arc<LogSettlement>,
    ) -> Self {
        Self {
            receiver,
            dropped,
            settlement,
            stderr: None,
            batch_records_max: LOG_BATCH_RECORDS_MAX,
            budget: RecordBudget::new(LOG_SUBSCRIPTION_BYTES_MAX),
        }
    }

    /// The drain of a process whose standard error is cut at `bound`: its stop records the
    /// bytes discarded there.
    pub(crate) fn with_stderr(mut self, bound: Arc<StderrBound>) -> Self {
        self.stderr = Some(bound);
        self
    }

    /// Shares the queue's byte capacity with retained batches and drop notices.
    pub(crate) fn with_budget(mut self, budget: RecordBudget) -> Self {
        self.budget = budget;
        self
    }

    /// A handle on this drain's lane that outlives the drain task.
    #[must_use]
    pub fn lane(&self) -> LogLane {
        LogLane {
            dropped: Arc::clone(&self.dropped),
            settlement: Arc::clone(&self.settlement),
            stderr: self.stderr.clone(),
        }
    }

    /// One queued record, without waiting; for a test that reads the queue without a
    /// store.
    ///
    /// # Errors
    ///
    /// Returns the queue's own answer when it holds no record or is closed.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn try_recv_record(&mut self) -> Result<LogRecord, mpsc::error::TryRecvError> {
        self.receiver.try_recv().map(|queued| queued.record)
    }

    /// Every record the queue holds now, oldest first, without waiting; for a test that
    /// asserts on what its [`ScopedRecorder`](crate::ScopedRecorder) captured.
    ///
    /// The queue holds at most [`LOG_QUEUE_RECORDS`](crate::LOG_QUEUE_RECORDS); a record
    /// sent while it was full is dropped and absent here.
    #[cfg(any(test, feature = "fixtures"))]
    #[must_use]
    pub fn queued_records(&mut self) -> Vec<LogRecord> {
        std::iter::from_fn(|| self.try_recv_record().ok()).collect()
    }

    /// Writes records until the queue closes, draining buffered records after
    /// cancellation.
    ///
    /// Records are written in batches: the task takes what the queue holds, waits
    /// `[logs] flush_interval` for more, and writes at most [`LOG_BATCH_RECORDS_MAX`] per
    /// call, `LOG_WORKSPACE_QUEUE_RECORDS` for a workspace consumer. Each write trims the
    /// store back to `retention_records`. A batch the store refuses is kept and retried
    /// every `[logs] retry_interval` until it lands; the first refusal of a batch is
    /// reported once to stderr, never into the queue the drain is failing to write.
    ///
    /// # Cancel safety
    ///
    /// Cancellation closes the receiving end and drains the bounded queue before return.
    /// Dropping the future loses the batch it holds and the queue; [`LogLane::unwritten`]
    /// counts them.
    pub async fn run(
        mut self,
        store: Arc<LogStore>,
        retention_records: u64,
        cancellation: CancellationToken,
    ) {
        let mut batch = Vec::with_capacity(self.batch_records_max);
        let mut closing = false;
        self.settlement.draining.store(true, Ordering::SeqCst);
        loop {
            let received = tokio::select! {
                biased;
                () = cancellation.cancelled(), if !closing => {
                    self.receiver.close();
                    closing = true;
                    continue;
                }
                received = self.receiver.recv_many(&mut batch, self.batch_records_max) => received,
            };
            if received == 0 {
                break;
            }
            if !closing
                && matches!(
                    self.wait_for_flush(&cancellation).await,
                    FlushReady::Cancelled
                )
            {
                self.receiver.close();
                closing = true;
            }
            while batch.len() < self.batch_records_max {
                match self.receiver.try_recv() {
                    Ok(queued) => batch.push(queued),
                    Err(_) => break,
                }
            }
            self.write_turn(&store, &mut batch, retention_records).await;
        }
        self.write_turn(&store, &mut batch, retention_records).await;
        self.settlement.draining.store(false, Ordering::SeqCst);
    }

    /// Waits for the accepted flush interval, a requesting read, or cancellation.
    async fn wait_for_flush(&self, cancellation: &CancellationToken) -> FlushReady {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => FlushReady::Cancelled,
            () = self.settlement.flush.notified() => FlushReady::Due,
            () = tokio::time::sleep(self.settlement.options.flush_interval) => FlushReady::Due,
        }
    }

    /// Writes one batch, and reports how far through the sequence the lane now is.
    ///
    /// The highest sequence in the batch is written through, and every lower one is
    /// written or was dropped, because the queue is first in, first out. The records move
    /// out of `batch` into one shared slice, and every attempt hands the writer thread that
    /// slice, so a retry copies no record.
    async fn write_turn(
        &self,
        store: &LogStore,
        batch: &mut Vec<QueuedRecord>,
        retention_records: u64,
    ) {
        self.note_drops(batch);
        if batch.is_empty() {
            return;
        }
        let written_through = batch
            .iter()
            .map(|queued| queued.sequence)
            .max()
            .unwrap_or(0);
        // The drop notice carries no sequence, because the sink never stamped it. Counting
        // it among the records finished with would let `finished` pass `accepted`, and a
        // read waiting on a record still queued would be released by that overshoot.
        let stamped = batch.iter().filter(|queued| queued.sequence != 0).count() as u64;
        let (records, bytes): (Vec<_>, Vec<_>) = batch
            .drain(..)
            .map(|queued| (queued.record, queued.bytes))
            .unzip();
        let records = Arc::new(crate::store::RetainedLogBatch {
            records: records.into(),
            _bytes: bytes,
        });
        write_retained_with(
            records.records.len(),
            self.settlement.options.retry_interval,
            || store.append_retained(Arc::clone(&records), retention_records),
        )
        .await;
        self.settlement.finish_written(stamped, written_through);
        drop(records);
    }

    /// Appends one record naming the drops so far, when there are any. The count is what
    /// a reader needs to know the run is missing records.
    fn note_drops(&self, batch: &mut Vec<QueuedRecord>) {
        if batch.len() == self.batch_records_max {
            return;
        }
        if self.dropped.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut notice = LogRecord::new(
            now_ms(),
            "warn",
            module_path!(),
            "logs",
            "logs.drain",
            "the log queue was full and dropped records",
            &format!("{{\"dropped\":{}}}", u64::MAX),
        );
        let Some(bytes) = self.budget.reserve(&notice) else {
            return;
        };
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        notice.fields = format!("{{\"dropped\":{dropped}}}");
        // The drain mints this one, so it carries no sequence of its own: sequence zero
        // is below every stamped record and never raises what the lane has written
        // through.
        batch.push(QueuedRecord {
            sequence: 0,
            bytes: Some(bytes),
            record: notice,
        });
    }
}

impl LogDrain {
    /// Hands every queued record to the consumer of the workspace it names, until the
    /// queue closes, handing on buffered records after cancellation.
    ///
    /// The routing drain writes nothing itself: it takes what the queue holds, at most
    /// [`LOG_BATCH_RECORDS_MAX`] at once, and finishes each record on this lane once it is
    /// handed on, dropped at a full consumer queue, or left to stderr because it names no
    /// workspace with a consumer. The queue is first in, first out, so the lane's
    /// `written_through` says every record up to it was handed on. Drops at the process
    /// queue stay counted on this lane, because no workspace store can carry them.
    ///
    /// # Cancel safety
    ///
    /// Cancellation closes the receiving end and hands on the bounded queue before
    /// return. Dropping the future loses the queue; [`LogLane::unwritten`] counts it.
    async fn route(mut self, routes: Arc<LogRoutes>, cancellation: CancellationToken) {
        let mut batch = Vec::with_capacity(LOG_BATCH_RECORDS_MAX);
        let mut closing = false;
        self.settlement.draining.store(true, Ordering::SeqCst);
        loop {
            let received = tokio::select! {
                biased;
                () = cancellation.cancelled(), if !closing => {
                    self.receiver.close();
                    closing = true;
                    continue;
                }
                received = self.receiver.recv_many(&mut batch, LOG_BATCH_RECORDS_MAX) => received,
            };
            if received == 0 {
                break;
            }
            let written_through = batch
                .iter()
                .map(|queued| queued.sequence)
                .max()
                .unwrap_or(0);
            let handed = batch.len() as u64;
            for queued in batch.drain(..) {
                routes.route(queued.record);
            }
            self.settlement.finish_written(handed, written_through);
        }
        self.settlement.draining.store(false, Ordering::SeqCst);
    }
}

/// The workspace consumers of one routing drain, by the workspace each writes for.
///
/// At most [`RunningLogDrain::WORKSPACE_CONSUMERS_MAX`] consumer budgets are retained,
/// including replaced consumers and their queued records or writer batches. The map's
/// lock is held for a lookup and a `try_send`, never across an await.
#[derive(Debug)]
struct LogRoutes {
    retention_records: u64,
    consumers: Mutex<WorkspaceConsumers>,
    options: LogDeliveryOptions,
}

/// Active routes and retained consumer budgets, admitted under the same lock.
#[derive(Debug, Default)]
struct WorkspaceConsumers {
    routes: BTreeMap<String, WorkspaceRoute>,
    budgets: Vec<Weak<Semaphore>>,
}

/// The sending end of one workspace consumer's queue, and its lane.
#[derive(Clone, Debug)]
struct WorkspaceRoute {
    sender: Sender<QueuedRecord>,
    dropped: Arc<AtomicU64>,
    settlement: Arc<LogSettlement>,
    budget: RecordBudget,
}

impl WorkspaceRoute {
    /// Queues one record for this consumer, counting a drop rather than waiting for room.
    ///
    /// The record takes its sequence on the consumer's lane before the send, as the
    /// capture layer stamps the process lane. A full queue adds one to the consumer's
    /// drop count, which its next batch reports in one record, and one to
    /// `log.queue.dropped` with `error.type` `queue_full`.
    fn send(&self, record: LogRecord) {
        let sequence = self.settlement.accept();
        let Some(bytes) = self.budget.reserve(&record) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.settlement.finish_dropped();
            LOG_QUEUE_DROPPED.labeled([QUEUE_FULL]).add(1);
            return;
        };
        match self.sender.try_send(QueuedRecord {
            sequence,
            record,
            bytes: Some(bytes),
        }) {
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.settlement.finish_dropped();
                LOG_QUEUE_DROPPED.labeled([QUEUE_FULL]).add(1);
            }
            Err(TrySendError::Closed(_)) => self.settlement.finish_dropped(),
            Ok(()) => {}
        }
    }
}

impl LogRoutes {
    #[cfg(test)]
    fn new(retention_records: u64) -> Self {
        Self::with_options(retention_records, LogDeliveryOptions::default())
    }

    fn with_options(retention_records: u64, options: LogDeliveryOptions) -> Self {
        Self {
            retention_records,
            consumers: Mutex::new(WorkspaceConsumers::default()),
            options,
        }
    }

    fn consumers(&self) -> MutexGuard<'_, WorkspaceConsumers> {
        self.consumers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands `record` to the consumer of the workspace it names; a record that names
    /// none, or one with no consumer, is left to stderr.
    fn route(&self, record: LogRecord) {
        let Some(workspace) = workspace_of(&record) else {
            return;
        };
        if let Some(route) = self.consumers().routes.get(&workspace) {
            route.send(record);
        }
    }

    /// Starts the consumer of `workspace`, writing into `store`, and routes the
    /// workspace's records to it from now on.
    ///
    /// A consumer already routed for `workspace` stops receiving: the new one replaces it.
    /// Answers `None` when [`RunningLogDrain::WORKSPACE_CONSUMERS_MAX`] consumer budgets
    /// are retained, including replaced consumers, queued records, and writer batches.
    /// A refusal leaves the current route unchanged.
    fn admit(
        self: &Arc<Self>,
        upstream: &Arc<LogSettlement>,
        workspace: &str,
        store: Arc<LogStore>,
    ) -> Option<RunningLogDrain> {
        let queue_records = self.options.queue_records.min(LOG_WORKSPACE_QUEUE_RECORDS);
        let (sender, receiver) = mpsc::channel(queue_records);
        let dropped = Arc::new(AtomicU64::new(0));
        let settlement = Arc::new(LogSettlement::with_options(self.options));
        let budget = RecordBudget::new(LOG_SUBSCRIPTION_BYTES_MAX / 8);
        {
            let mut consumers = self.consumers();
            consumers
                .budgets
                .retain(|budget| budget.upgrade().is_some());
            if consumers.budgets.len() >= RunningLogDrain::WORKSPACE_CONSUMERS_MAX {
                return None;
            }
            consumers.budgets.push(budget.downgrade());
            consumers.routes.insert(
                workspace.to_owned(),
                WorkspaceRoute {
                    sender,
                    dropped: Arc::clone(&dropped),
                    settlement: Arc::clone(&settlement),
                    budget: budget.clone(),
                },
            );
        }
        let mut consumer = LogDrain::new(receiver, dropped, settlement).with_budget(budget);
        consumer.batch_records_max = queue_records;
        let mut running = RunningLogDrain::spawn(consumer, store, self.retention_records);
        running.route = Some(RouteBinding {
            routes: Arc::clone(self),
            upstream: Arc::clone(upstream),
            workspace: workspace.to_owned(),
        });
        Some(running)
    }

    /// The lane of the consumer routed for `workspace`.
    fn settlement_of(&self, workspace: &str) -> Option<Arc<LogSettlement>> {
        self.consumers()
            .routes
            .get(workspace)
            .map(|route| Arc::clone(&route.settlement))
    }

    /// Stops routing records to the consumer whose lane is `settlement`, unless another
    /// consumer replaced it for `workspace`. Its queue closes once the routing drain holds
    /// no clone of the sending end.
    fn release(&self, workspace: &str, settlement: &Arc<LogSettlement>) {
        let mut consumers = self.consumers();
        if consumers
            .routes
            .get(workspace)
            .is_some_and(|route| Arc::ptr_eq(&route.settlement, settlement))
        {
            consumers.routes.remove(workspace);
        }
    }
}

/// The workspace `record` belongs to: the first `workspace` string of
/// [`WORKSPACE_FIELD_POINTERS`] in its fields. Fields that do not parse, such as fields cut
/// at their bound, name none.
fn workspace_of(record: &LogRecord) -> Option<String> {
    if !record.fields().contains("\"workspace\"") {
        return None;
    }
    let fields: serde_json::Value = serde_json::from_str(record.fields()).ok()?;
    WORKSPACE_FIELD_POINTERS
        .iter()
        .find_map(|pointer| fields.pointer(pointer).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

/// What a workspace consumer's stop undoes: its route, on the routing drain upstream.
#[derive(Debug)]
struct RouteBinding {
    routes: Arc<LogRoutes>,
    upstream: Arc<LogSettlement>,
    workspace: String,
}

/// Runs `append` until one attempt lands, waiting [`LOG_WRITE_RETRY_INTERVAL`] after
/// each refusal. The first refusal of the batch of `records` records is reported to
/// stderr; the later ones repeat it.
#[cfg(test)]
async fn write_retained<Append, Attempt>(records: usize, append: Append)
where
    Append: FnMut() -> Attempt,
    Attempt: Future<Output = Result<u64, RiftError>>,
{
    write_retained_with(records, LOG_WRITE_RETRY_INTERVAL, append).await;
}

/// Retries one retained batch under the accepted interval until a write succeeds.
async fn write_retained_with<Append, Attempt>(
    records: usize,
    retry_interval: Duration,
    mut append: Append,
) where
    Append: FnMut() -> Attempt,
    Attempt: Future<Output = Result<u64, RiftError>>,
{
    let mut refused = false;
    while let Err(error) = append().await {
        if !refused {
            refused = true;
            eprintln!(
                "rift: the log store refused a batch of {records}; the log drain keeps it \
                 and retries every {retry} ms: {error}{cause}",
                retry = retry_interval.as_millis(),
                cause = caused_by(&error)
            );
        }
        tokio::time::sleep(retry_interval).await;
    }
}

/// One failure's causes, in order, as text a reader can act on. The wrapped label alone
/// names the layer that refused; the chain names what the database actually said.
fn caused_by(error: &(dyn Error + 'static)) -> String {
    let mut rendered = String::new();
    for cause in causes(error) {
        let _ = write!(rendered, ": {cause}");
    }
    rendered
}

/// A log drain task a stop joins, with its own stop token and its lane.
///
/// The drain stops on its own token rather than the serving one, so records the stop
/// stages emit after serving ended still reach the queue it drains.
#[derive(Debug)]
pub struct RunningLogDrain {
    task: JoinHandle<()>,
    lane: LogLane,
    stop: CancellationToken,
    /// The route a workspace consumer is reached through; `None` for every other drain.
    route: Option<RouteBinding>,
}

impl RunningLogDrain {
    /// Most consumer budgets one routing drain retains at once, including replaced
    /// consumers, queued records, and writer batches: at least the workspaces one
    /// repository process retains, `SERVER_WORKSPACES_MAX`.
    pub const WORKSPACE_CONSUMERS_MAX: usize = 64;

    /// Starts `drain` writing into `store`, trimming it back to `retention_records`.
    #[must_use]
    pub fn spawn(drain: LogDrain, store: Arc<LogStore>, retention_records: u64) -> Self {
        let lane = drain.lane();
        let stop = CancellationToken::new();
        // Set before the task first runs, so a read or a consumer stop that comes first
        // still waits for it.
        drain.settlement.draining.store(true, Ordering::SeqCst);
        let task = tokio::spawn(drain.run(store, retention_records, stop.clone()));
        Self {
            task,
            lane,
            stop,
            route: None,
        }
    }

    /// Starts `drain` routing each record to the consumer of the workspace it names, for
    /// a process that serves several workspaces and opens no store of its own.
    ///
    /// [`Self::for_workspace`] starts a consumer; each trims its store back to
    /// `retention_records`. A record that names no workspace with a consumer reaches
    /// stderr alone.
    #[must_use]
    pub fn spawn_routed(drain: LogDrain, retention_records: u64) -> Self {
        let lane = drain.lane();
        let stop = CancellationToken::new();
        let routes = Arc::new(LogRoutes::with_options(
            retention_records,
            drain.settlement.options,
        ));
        let _ = drain.settlement.routes.set(Arc::clone(&routes));
        // Set before the task first runs, so a read or a consumer stop that comes first
        // still waits for it.
        drain.settlement.draining.store(true, Ordering::SeqCst);
        let task = tokio::spawn(drain.route(routes, stop.clone()));
        Self {
            task,
            lane,
            stop,
            route: None,
        }
    }

    /// Starts the consumer of `workspace`, writing the records that name it into `store`,
    /// on the routing drain of the calling thread's dispatcher.
    ///
    /// `workspace` is spelled as the `workspace` field carries it: the workspace root's
    /// display form. Answers `None` when that dispatcher has no routing drain, and when
    /// [`Self::WORKSPACE_CONSUMERS_MAX`] consumer budgets remain retained. The consumer's
    /// [`Self::stop`] first waits, by its deadline, for the routing drain to hand on what
    /// it had taken, then ends the route and flushes.
    #[must_use]
    pub fn for_workspace(workspace: &str, store: Arc<LogStore>) -> Option<Self> {
        let upstream = installed_settlement()?;
        let routes = Arc::clone(upstream.routes.get()?);
        routes.admit(&upstream, workspace, store)
    }

    /// Stops the drain and joins it by `deadline`; answers the records left unwritten
    /// when the drain had to be aborted.
    ///
    /// A workspace consumer first waits, by `deadline`, for its routing drain to hand on
    /// the records it had taken when the stop began, then leaves the routes, so the
    /// records its workspace emitted before the stop reach its store.
    ///
    /// The drain closes its queue, then flushes what it holds, retrying a refused batch,
    /// within what is left of `deadline`. A drain that outlasts it is aborted, and the
    /// "log drain outlasted the stop deadline" warning carries `unwritten`: the records
    /// the lane accepted and never wrote, its held batch and queue included. The held batch
    /// and queue are added to `log.queue.dropped` with `error.type` `unwritten`; the full
    /// queue's drops are there already as `queue_full`.
    ///
    /// When the process's standard error is cut at `[logs] stderr_limit`, the stop first
    /// records `standard error bytes discarded`, `INFO` with `stderr_limit` and the bytes
    /// `discarded` so far, `WARN` when that count is not zero, so the drain writes the
    /// count with its last batch.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future after the stop token is cancelled leaves the drain flushing
    /// on its own, unjoined.
    pub async fn stop(self, deadline: Instant) -> Option<u64> {
        let Self {
            mut task,
            lane,
            stop,
            route,
        } = self;
        if let Some(route) = route {
            route.upstream.settle_by(deadline).await;
            route.routes.release(&route.workspace, &lane.settlement);
        }
        if let Some(bound) = &lane.stderr {
            let (stderr_limit, discarded) = (bound.limit(), bound.discarded());
            if discarded == 0 {
                tracing::info!(
                    component = "logs",
                    stderr_limit,
                    discarded,
                    "standard error bytes discarded"
                );
            } else {
                tracing::warn!(
                    component = "logs",
                    stderr_limit,
                    discarded,
                    "standard error bytes discarded"
                );
            }
        }
        stop.cancel();
        match tokio::time::timeout_at(deadline, &mut task).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => {
                tracing::warn!(component = "logs", %error, "log drain task failed");
                None
            }
            Err(_) => {
                task.abort();
                let _ = task.await;
                // A full queue's drops are already in `queue_full`; `unwritten` adds the
                // records the drain held or had queued.
                LOG_QUEUE_DROPPED
                    .labeled([UNWRITTEN])
                    .add(lane.never_written());
                let unwritten = lane.unwritten();
                tracing::warn!(
                    component = "logs",
                    unwritten,
                    "log drain outlasted the stop deadline"
                );
                Some(unwritten)
            }
        }
    }
}

#[cfg(test)]
mod tests;
