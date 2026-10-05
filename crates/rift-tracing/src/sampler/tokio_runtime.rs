//! The runtime sample: the Tokio runtime the sampler runs on, read once per tick.
//!
//! Every reading comes from Tokio's stable `RuntimeMetrics`, so the sample needs no
//! `tokio_unstable` build: the worker count, the live tasks, the global queue depth, and each
//! worker's busy time and park count. A worker whose busy time fills the whole interval while
//! the global queue grows is a runtime worker something blocked.

use std::time::Duration;

use tokio::runtime::RuntimeMetrics;

use crate::metrics::{Counter, Gauge, MetricValues};

/// `tokio.runtime.worker.count`: the runtime's worker threads.
const RUNTIME_WORKERS: Gauge<u64, 0> =
    Gauge::declare("tokio.runtime.worker.count", "{thread}", &[]);
/// `tokio.runtime.task.count`: the tasks alive in the runtime.
const RUNTIME_TASKS: Gauge<u64, 0> = Gauge::declare("tokio.runtime.task.count", "{task}", &[]);
/// `tokio.runtime.global_queue.length`: the tasks waiting in the runtime's global queue.
const RUNTIME_GLOBAL_QUEUE: Gauge<u64, 0> =
    Gauge::declare("tokio.runtime.global_queue.length", "{task}", &[]);
/// `tokio.runtime.worker.busy.time`: the time every worker spent busy, in seconds.
const RUNTIME_BUSY: Counter<0> = Counter::declare("tokio.runtime.worker.busy.time", "s", &[]);
/// `tokio.runtime.worker.busy.time.max`: the busy time of the busiest worker over the last
/// interval, in seconds.
const RUNTIME_BUSY_MAX: Gauge<f64, 0> =
    Gauge::declare("tokio.runtime.worker.busy.time.max", "s", &[]);
/// `tokio.runtime.worker.parks`: the times every worker parked for lack of work.
const RUNTIME_PARKS: Counter<0> = Counter::declare("tokio.runtime.worker.parks", "{park}", &[]);

/// One read of a Tokio runtime.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RuntimeReading {
    /// Worker threads; one for a current-thread runtime.
    pub(crate) workers: usize,
    /// Tasks spawned and not yet finished.
    pub(crate) alive_tasks: usize,
    /// Tasks waiting in the global queue.
    pub(crate) global_queue_depth: usize,
    /// Each worker's busy time since the runtime started, by worker index; empty on a
    /// target without 64-bit atomics, where Tokio does not count it.
    pub(crate) busy: Vec<Duration>,
    /// Each worker's park count since the runtime started, by worker index; empty where
    /// [`Self::busy`] is.
    pub(crate) parks: Vec<u64>,
}

impl RuntimeReading {
    /// Reads `metrics`. The reads are loads of counters Tokio keeps; none waits.
    pub(crate) fn of(metrics: &RuntimeMetrics) -> Self {
        let workers = metrics.num_workers();
        #[cfg(target_has_atomic = "64")]
        let (busy, parks) = (0..workers)
            .map(|worker| {
                (
                    metrics.worker_total_busy_duration(worker),
                    metrics.worker_park_count(worker),
                )
            })
            .unzip();
        #[cfg(not(target_has_atomic = "64"))]
        let (busy, parks) = (Vec::new(), Vec::new());
        Self {
            workers,
            alive_tasks: metrics.num_alive_tasks(),
            global_queue_depth: metrics.global_queue_depth(),
            busy,
            parks,
        }
    }
}

/// The previous runtime reading the next one's changes are taken against.
#[derive(Debug, Default)]
pub(crate) struct RuntimeSeries {
    previous: Option<RuntimeReading>,
}

impl RuntimeSeries {
    /// Records `reading` into the runtime instruments held in `values`: the counts as
    /// gauges, and, from the second reading on, the busy time and parks since the previous
    /// reading, with the busiest worker's share. The first reading sets the base and
    /// records no change; a worker whose totals went backwards adds none.
    pub(crate) fn observe(&mut self, reading: RuntimeReading, values: &MetricValues) {
        for (gauge, count) in [
            (RUNTIME_WORKERS, reading.workers),
            (RUNTIME_TASKS, reading.alive_tasks),
            (RUNTIME_GLOBAL_QUEUE, reading.global_queue_depth),
        ] {
            gauge.record_into(values, [], u64::try_from(count).unwrap_or(u64::MAX));
        }
        if let Some(previous) = self.previous.as_ref() {
            let busy: Vec<Duration> = reading
                .busy
                .iter()
                .zip(&previous.busy)
                .map(|(now, before)| now.saturating_sub(*before))
                .collect();
            if !busy.is_empty() {
                let total: Duration = busy.iter().sum();
                RUNTIME_BUSY.add_into(values, [], total.as_secs_f64());
                let busiest = busy.iter().max().copied().unwrap_or_default();
                RUNTIME_BUSY_MAX.record_into(values, [], busiest.as_secs_f64());
            }
            let parks: u64 = reading
                .parks
                .iter()
                .zip(&previous.parks)
                .map(|(now, before)| now.saturating_sub(*before))
                .sum();
            if !reading.parks.is_empty() {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "a park count past 2^53 in one interval is not a runtime's"
                )]
                let parks = parks as f64;
                RUNTIME_PARKS.add_into(values, [], parks);
            }
        }
        self.previous = Some(reading);
    }
}
