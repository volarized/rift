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

use std::error::Error;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use rift_error::{RiftError, causes};
use tokio::sync::mpsc::{self, Receiver};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::capture::{LogSink, now_ms};
use crate::record::{LOG_BATCH_RECORDS_MAX, LogRecord};
use crate::store::LogStore;

/// Wall-clock span the drain waits for more records before writing what it holds.
const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
/// Longest a `rift://logs` read waits for the drain to write through the sequence the
/// sink had stamped when the read began. A read past this answers with what the store
/// holds: a log read never fails, and never hangs, because the log drain is slow.
pub const LOG_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Wall-clock span between two attempts at a batch the store refused.
const LOG_WRITE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// One record on its way to the drain, under the sequence the sink stamped on it.
///
/// The sequence is what a read waits on. A count cannot serve: a full queue drops the
/// newest record while older ones are still queued, so the number of records the drain
/// has finished with says nothing about which ones.
#[derive(Debug)]
pub(crate) struct QueuedRecord {
    pub(crate) sequence: u64,
    pub(crate) record: LogRecord,
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
}

impl Default for LogSettlement {
    fn default() -> Self {
        Self {
            accepted: AtomicU64::new(0),
            progress: watch::Sender::new(LaneProgress::default()),
            flush: Notify::new(),
            draining: AtomicBool::new(false),
        }
    }
}

impl LogSettlement {
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

    /// Waits for the drain to write through the sequence this read stamped on entry.
    ///
    /// Returns at once when no drain is running and when the lane is already there.
    /// Past [`LOG_SETTLE_TIMEOUT`] the read proceeds with whatever the store holds.
    async fn settle_for_read(&self) {
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
        let _ = tokio::time::timeout(LOG_SETTLE_TIMEOUT, reached).await;
    }
}

/// Waits for the calling thread's log lane to write through what it has stamped.
///
/// Every `rift://logs` read calls this before it opens the store. The lane is the
/// [`LogSink`] layer of the thread's current `tracing` dispatcher. A process can build
/// more than one lane - under `cargo test` every test is a thread of one process and
/// builds its own - and the dispatcher is what decides where a thread's records go. A
/// process that records nothing - every command but the foreground server - installs no
/// sink, and its log reads wait for nothing.
///
/// # Cancel safety
///
/// Dropping the future ends the wait and changes nothing.
pub async fn settle_for_read() {
    let installed = tracing::dispatcher::get_default(|dispatch| {
        dispatch
            .downcast_ref::<LogSink>()
            .map(|sink| Arc::clone(&sink.settlement))
    });
    if let Some(settlement) = installed {
        settlement.settle_for_read().await;
    }
}

/// What a stop asks of the log lane once its drain is joined or aborted: how many
/// records it took and never wrote.
///
/// A clone taken before the drain runs keeps answering after the drain task is aborted.
#[derive(Clone, Debug)]
pub struct LogLane {
    dropped: Arc<AtomicU64>,
    settlement: Arc<LogSettlement>,
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
        let accepted = self.settlement.accepted.load(Ordering::SeqCst);
        let finished = self.settlement.progress.borrow().finished;
        accepted
            .saturating_sub(finished)
            .saturating_add(self.dropped.load(Ordering::Relaxed))
    }
}

/// The queue's reading end, and the task that writes it into the store.
#[derive(Debug)]
pub struct LogDrain {
    receiver: Receiver<QueuedRecord>,
    dropped: Arc<AtomicU64>,
    settlement: Arc<LogSettlement>,
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
        }
    }

    /// A handle on this drain's lane that outlives the drain task.
    #[must_use]
    pub fn lane(&self) -> LogLane {
        LogLane {
            dropped: Arc::clone(&self.dropped),
            settlement: Arc::clone(&self.settlement),
        }
    }

    /// One queued record, without waiting; for a test that reads the queue without a
    /// store.
    ///
    /// # Errors
    ///
    /// Returns the queue's own answer when it holds no record or is closed.
    pub fn try_recv_record(&mut self) -> Result<LogRecord, mpsc::error::TryRecvError> {
        self.receiver.try_recv().map(|queued| queued.record)
    }

    /// Writes records until the queue closes, draining buffered records after
    /// cancellation.
    ///
    /// Records are written in batches: the task takes what the queue holds, waits
    /// [`LOG_FLUSH_INTERVAL`] for more, and writes at most [`LOG_BATCH_RECORDS_MAX`] per
    /// call. Each write trims the store back to `retention_records`. A batch the store
    /// refuses is kept and retried every [`LOG_WRITE_RETRY_INTERVAL`] until it lands;
    /// the first refusal of a batch is reported once to stderr, never into the queue the
    /// drain is failing to write.
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
            tokio::select! {
                biased;
                () = cancellation.cancelled(), if !closing => {
                    self.receiver.close();
                    closing = true;
                }
                () = self.settlement.flush.notified(), if !closing => {}
                () = tokio::time::sleep(LOG_FLUSH_INTERVAL), if !closing => {}
                else => {}
            }
            while batch.len() < LOG_BATCH_RECORDS_MAX {
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

    /// Writes one batch, and reports how far through the sequence the lane now is.
    ///
    /// The highest sequence in the batch is written through, and every lower one is
    /// written or was dropped, because the queue is first in, first out. The records move
    /// out of `batch` into the write, so the drain holds one copy of them while it
    /// retries.
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
        let records: Vec<LogRecord> = batch.drain(..).map(|queued| queued.record).collect();
        let held = records.as_slice();
        write_retained(held.len(), move || store.append(held, retention_records)).await;
        self.settlement.finish_written(stamped, written_through);
    }

    /// Appends one record naming the drops so far, when there are any. The count is what
    /// a reader needs to know the run is missing records.
    fn note_drops(&self, batch: &mut Vec<QueuedRecord>) {
        if batch.len() == LOG_BATCH_RECORDS_MAX {
            return;
        }
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped == 0 {
            return;
        }
        // The drain mints this one, so it carries no sequence of its own: sequence zero
        // is below every stamped record and never raises what the lane has written
        // through.
        batch.push(QueuedRecord {
            sequence: 0,
            record: LogRecord::new(
                now_ms(),
                "warn",
                module_path!(),
                "logs",
                "logs.drain",
                "the log queue was full and dropped records",
                &format!("{{\"dropped\":{dropped}}}"),
            ),
        });
    }
}

/// Runs `append` until one attempt lands, waiting [`LOG_WRITE_RETRY_INTERVAL`] after
/// each refusal. The first refusal of the batch of `records` records is reported to
/// stderr; the later ones repeat it.
async fn write_retained<Append, Attempt>(records: usize, mut append: Append)
where
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
                retry = LOG_WRITE_RETRY_INTERVAL.as_millis(),
                cause = caused_by(&error)
            );
        }
        tokio::time::sleep(LOG_WRITE_RETRY_INTERVAL).await;
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
}

impl RunningLogDrain {
    /// Starts `drain` writing into `store`, trimming it back to `retention_records`.
    #[must_use]
    pub fn spawn(drain: LogDrain, store: Arc<LogStore>, retention_records: u64) -> Self {
        let lane = drain.lane();
        let stop = CancellationToken::new();
        let task = tokio::spawn(drain.run(store, retention_records, stop.clone()));
        Self { task, lane, stop }
    }

    /// Stops the drain and joins it by `deadline`; answers the records left unwritten
    /// when the drain had to be aborted.
    ///
    /// The drain closes its queue, then flushes what it holds, retrying a refused batch,
    /// within what is left of `deadline`. A drain that outlasts it is aborted, and the
    /// "log drain outlasted the stop deadline" warning carries `unwritten`: the records
    /// the lane accepted and never wrote, its held batch and queue included.
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
        } = self;
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
