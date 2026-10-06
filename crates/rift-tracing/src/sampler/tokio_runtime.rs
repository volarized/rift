//! The Tokio runtime readings: the runtime that installed the subscriber, read each time the
//! meter's reader collects.
//!
//! Every reading comes from Tokio's stable `RuntimeMetrics`, so it needs no `tokio_unstable`
//! build: the worker count, the live tasks, the global queue depth, and the busy time and
//! park count summed over the workers. Each is a load of a counter Tokio keeps; none waits.
//! A worker busy time that grows by the whole interval while the global queue grows names a
//! runtime worker something blocked.

use opentelemetry::metrics::Meter;
use tokio::runtime::RuntimeMetrics;

use crate::metrics::with_meter;

/// One count the runtime keeps: its instrument name, unit, and how it is read.
type RuntimeCount = (&'static str, &'static str, fn(&RuntimeMetrics) -> usize);

/// `value` as an up-down counter observes it.
fn signed(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Registers the readings of the runtime `metrics` describes with the installed meter;
/// answers whether a meter was installed.
///
/// Each callback loads Tokio's counters: one per worker for the busy time and the parks,
/// one for each count. On a target without 64-bit atomics Tokio counts no busy time and no
/// park, and those two readings report nothing.
pub(crate) fn observe_runtime(metrics: &RuntimeMetrics) -> bool {
    with_meter(|meter| register(meter, metrics))
}

fn register(meter: &Meter, metrics: &RuntimeMetrics) {
    let counts: [RuntimeCount; 3] = [
        (
            "tokio.runtime.worker.count",
            "{thread}",
            RuntimeMetrics::num_workers,
        ),
        (
            "tokio.runtime.task.count",
            "{task}",
            RuntimeMetrics::num_alive_tasks,
        ),
        (
            "tokio.runtime.global_queue.length",
            "{task}",
            RuntimeMetrics::global_queue_depth,
        ),
    ];
    for (name, unit, count) in counts {
        let runtime = metrics.clone();
        let _count = meter
            .i64_observable_up_down_counter(name)
            .with_unit(unit)
            .with_callback(move |instrument| instrument.observe(signed(count(&runtime)), &[]))
            .build();
    }
    #[cfg(target_has_atomic = "64")]
    {
        let runtime = metrics.clone();
        let _busy = meter
            .f64_observable_counter("tokio.runtime.worker.busy.time")
            .with_unit("s")
            .with_callback(move |instrument| {
                let busy: std::time::Duration = (0..runtime.num_workers())
                    .map(|worker| runtime.worker_total_busy_duration(worker))
                    .sum();
                instrument.observe(busy.as_secs_f64(), &[]);
            })
            .build();
        let runtime = metrics.clone();
        let _parks = meter
            .u64_observable_counter("tokio.runtime.worker.parks")
            .with_unit("{park}")
            .with_callback(move |instrument| {
                let parks: u64 = (0..runtime.num_workers())
                    .map(|worker| runtime.worker_park_count(worker))
                    .sum();
                instrument.observe(parks, &[]);
            })
            .build();
    }
}
