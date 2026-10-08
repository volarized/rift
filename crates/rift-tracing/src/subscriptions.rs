//! Independent bounded subscriptions to the process's captured records.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use rift_error::{RiftError, errors};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::capture::{LOG_QUEUE_DROPPED, QUEUE_FULL};
use crate::record::{LOG_BATCH_RECORDS_MAX, LogRecord};

/// Most subscriptions of one capture, including its persistence subscription.
pub const LOG_SUBSCRIPTIONS_MAX: usize = 16;
/// Bytes one subscription retains, including records handed to its consumer.
///
/// Accounting includes record strings, record and publication values, and the shared
/// record header. Allocator and channel storage overhead is outside this byte count.
/// A consumer holding a batch spends the same budget as its queue. With
/// [`LOG_SUBSCRIPTIONS_MAX`] subscriptions, retained records cost at most 128 MiB,
/// plus the bounded channel slots and the shared record's allocation overhead.
pub const LOG_SUBSCRIPTION_BYTES_MAX: usize = 8 << 20;

/// The byte capacity a queue and its retained records share.
#[derive(Clone, Debug)]
pub(crate) struct RecordBudget(Arc<Semaphore>);

impl RecordBudget {
    pub(crate) fn new(bytes: usize) -> Self {
        Self(Arc::new(Semaphore::new(bytes)))
    }

    pub(crate) fn downgrade(&self) -> Weak<Semaphore> {
        Arc::downgrade(&self.0)
    }

    /// Reserves a record's retained storage without waiting for the consumer.
    pub(crate) fn reserve(&self, record: &LogRecord) -> Option<OwnedSemaphorePermit> {
        let bytes = record_bytes(record);
        let bytes = u32::try_from(bytes).ok()?;
        Arc::clone(&self.0).try_acquire_many_owned(bytes).ok()
    }
}

/// The record's bounded strings and the storage held beside them in a queue.
fn record_bytes(record: &LogRecord) -> usize {
    std::mem::size_of::<LogPublication>()
        + std::mem::size_of::<LogRecord>()
        + 2 * std::mem::size_of::<usize>()
        + record.level().len()
        + record.target().len()
        + record.component().len()
        + record.operation().len()
        + record.message().len()
        + record.fields().len()
}

/// One published record, with its position and retained byte capacity.
///
/// The record is shared across subscriptions. Dropping this publication releases
/// its subscription's bytes, so a retained batch remains inside that byte bound.
#[derive(Debug)]
#[must_use = "dropping a publication releases its subscription's byte capacity"]
pub struct LogPublication {
    sequence: u64,
    record: Arc<LogRecord>,
    _bytes: OwnedSemaphorePermit,
}

impl LogPublication {
    /// The position in this capture's publication order, including dropped records.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The immutable record this publication carries.
    #[must_use]
    pub fn record(&self) -> &LogRecord {
        &self.record
    }
}

/// One subscription's sending end, retained capacity, and independent loss count.
#[derive(Debug)]
struct SubscriptionQueue {
    sender: Sender<LogPublication>,
    budget: RecordBudget,
    dropped: Arc<AtomicU64>,
}

impl SubscriptionQueue {
    fn send(&self, sequence: u64, record: &Arc<LogRecord>) {
        let admitted = self.budget.reserve(record).is_some_and(|bytes| {
            self.sender
                .try_send(LogPublication {
                    sequence,
                    record: Arc::clone(record),
                    _bytes: bytes,
                })
                .is_ok()
        });
        if !admitted {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            LOG_QUEUE_DROPPED.add_built([QUEUE_FULL], 1);
        }
    }
}

/// One admission slot, retained while its queue or publications own byte capacity.
#[derive(Debug)]
struct SubscriptionSlot {
    queue: Option<SubscriptionQueue>,
    budget: Weak<Semaphore>,
}

/// The bounded subscribers and the lock that orders registration and publication.
#[derive(Debug)]
struct Publications {
    enabled: bool,
    queues: Vec<SubscriptionSlot>,
}

/// The captured record stream exposed by [`crate::TracingRuntime::logs`].
///
/// Registration and publication hold one short lock, never a consumer wait. Every
/// subscription receives the same order from registration onward, without replay.
#[derive(Clone, Debug)]
pub struct LogStream {
    publications: Arc<Mutex<Publications>>,
    queue_records: usize,
}

impl LogStream {
    pub(crate) fn new(queue_records: usize, enabled: bool) -> Self {
        Self {
            publications: Arc::new(Mutex::new(Publications {
                enabled,
                queues: Vec::new(),
            })),
            queue_records,
        }
    }

    fn publications(&self) -> MutexGuard<'_, Publications> {
        self.publications
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers one independent subscription, without replaying earlier records.
    ///
    /// Each queue holds at most `[logs] queue_records` records and shares
    /// [`LOG_SUBSCRIPTION_BYTES_MAX`] bytes with publications retained by its consumer.
    ///
    /// # Errors
    ///
    /// Returns `tracing.log_stream_unavailable` when capture is disabled or closed,
    /// or `tracing.log_subscription_limit` when [`LOG_SUBSCRIPTIONS_MAX`] subscriptions,
    /// including persistence and closed subscriptions with retained publications,
    /// already hold admission.
    pub fn subscribe(&self) -> Result<LogSubscription, RiftError> {
        let mut publications = self.publications();
        if !publications.enabled {
            return errors::tracing::log_stream_unavailable().fail();
        }
        let slot = publications
            .queues
            .iter()
            .position(|slot| slot.queue.is_none() && slot.budget.upgrade().is_none());
        let slot = match slot {
            Some(slot) => slot,
            None if publications.queues.len() < LOG_SUBSCRIPTIONS_MAX - 1 => {
                publications.queues.push(SubscriptionSlot {
                    queue: None,
                    budget: Weak::new(),
                });
                publications.queues.len() - 1
            }
            None => {
                return errors::tracing::log_subscription_limit()
                    .observed((LOG_SUBSCRIPTIONS_MAX + 1) as u64)
                    .maximum(LOG_SUBSCRIPTIONS_MAX as u64)
                    .fail();
            }
        };
        let (sender, receiver) = mpsc::channel(self.queue_records);
        let dropped = Arc::new(AtomicU64::new(0));
        let budget = RecordBudget::new(LOG_SUBSCRIPTION_BYTES_MAX);
        publications.queues[slot] = SubscriptionSlot {
            budget: budget.downgrade(),
            queue: Some(SubscriptionQueue {
                sender,
                budget,
                dropped: Arc::clone(&dropped),
            }),
        };
        Ok(LogSubscription {
            receiver,
            dropped,
            publications: Arc::downgrade(&self.publications),
            slot,
        })
    }

    /// Runs the persistence send and fan-out under one publication order.
    pub(crate) fn publish(&self, record: LogRecord, persist: impl FnOnce(LogRecord) -> u64) {
        let publications = self.publications();
        if publications.queues.iter().all(|slot| slot.queue.is_none()) {
            persist(record);
            return;
        }
        let record = Arc::new(record);
        let sequence = persist((*record).clone());
        for queue in publications
            .queues
            .iter()
            .filter_map(|slot| slot.queue.as_ref())
        {
            queue.send(sequence, &record);
        }
    }

    /// Ends delivery and unregisters sending ends; buffered publications remain readable.
    pub(crate) fn close(&self) {
        let mut publications = self.publications();
        publications.enabled = false;
        publications.queues.clear();
    }
}

/// One independent queue and its loss count.
///
/// Dropping the subscription unregisters it and releases its queued records.
/// Publications already returned keep their byte capacity and admission until their
/// owner drops them.
#[derive(Debug)]
#[must_use = "dropping a subscription unregisters it and releases its queued records"]
pub struct LogSubscription {
    receiver: Receiver<LogPublication>,
    dropped: Arc<AtomicU64>,
    publications: Weak<Mutex<Publications>>,
    slot: usize,
}

impl LogSubscription {
    /// Records this subscription lost because its record or byte capacity was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Waits for one publication; after closure, drains buffered publications first.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future removes no publication from the queue.
    pub async fn recv(&mut self) -> Option<LogPublication> {
        self.receiver.recv().await
    }

    /// Waits for a bounded batch, at most [`LOG_BATCH_RECORDS_MAX`] publications.
    ///
    /// Already returned publications spend the subscription's byte capacity, so a
    /// consumer must release them before delivery can fill that capacity again.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future removes no publication from the queue.
    pub async fn recv_batch(&mut self) -> Vec<LogPublication> {
        let mut batch = Vec::new();
        self.receiver
            .recv_many(&mut batch, LOG_BATCH_RECORDS_MAX)
            .await;
        batch
    }

    /// Closes this subscription to new delivery, preserving its buffered publications.
    pub fn close(&mut self) {
        self.receiver.close();
        self.unregister();
    }

    fn unregister(&self) {
        if let Some(publications) = self.publications.upgrade() {
            let mut publications = publications.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(slot) = publications.queues.get_mut(self.slot)
                && slot
                    .queue
                    .as_ref()
                    .is_some_and(|queue| Arc::ptr_eq(&queue.dropped, &self.dropped))
            {
                slot.queue = None;
            }
        }
    }
}

impl Drop for LogSubscription {
    fn drop(&mut self) {
        self.unregister();
    }
}

#[cfg(test)]
mod tests;
