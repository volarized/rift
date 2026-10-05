//! The process sampler: one refresh of the current process per tick, published as a sample.
//!
//! A tick reads the process once on the blocking pool, through `sysinfo`, and never walks
//! another process. The sample it publishes carries what the platform reported and the
//! changes since the previous one; a reading the platform does not report stays absent and
//! never becomes zero. The sampler records each sample into the process instruments, and
//! [`ProcessGauge::current`](crate::ProcessGauge::current) reads the latest one without an
//! OS read of its own.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::capture::now_ms;
use crate::flight::{FlightTable, publish_stalled};
use crate::measurement::monotonic_now;
use crate::metrics::{Counter, Gauge, MetricValues, metrics};
use crate::snapshot::{self, SnapshotSeries};

/// The shortest interval the sampler refreshes the process at. CPU usage is the change of
/// CPU time over the wall time between two refreshes, and `sysinfo` reads it reliably only
/// with at least its `MINIMUM_CPU_UPDATE_INTERVAL` between them, 200 ms on Linux, macOS,
/// and Windows; a shorter interval samples at this one.
pub const PROCESS_SAMPLE_INTERVAL_MIN: Duration = Duration::from_millis(200);
const _: () = assert!(
    sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.as_millis() <= PROCESS_SAMPLE_INTERVAL_MIN.as_millis()
);

/// `process.memory.virtual`: the virtual memory size in bytes.
const PROCESS_MEMORY_VIRTUAL: Gauge<u64, 0> = Gauge::declare("process.memory.virtual", "By", &[]);
/// `process.cpu.time`: CPU time the process consumed, in seconds.
const PROCESS_CPU_TIME: Counter<0> = Counter::declare("process.cpu.time", "s", &[]);
/// Open file descriptors: `process.unix.file_descriptor.count`.
#[cfg(not(windows))]
const PROCESS_OPEN_FILES: Gauge<u64, 0> = Gauge::declare(
    "process.unix.file_descriptor.count",
    "{file_descriptor}",
    &[],
);
/// Open handles of every kind: `process.windows.handle.count`.
#[cfg(windows)]
const PROCESS_OPEN_FILES: Gauge<u64, 0> =
    Gauge::declare("process.windows.handle.count", "{handle}", &[]);
/// `process.disk.io`: bytes read and written, by `disk.io.direction`.
const PROCESS_DISK_IO: Counter<1> =
    Counter::declare("process.disk.io", "By", &["disk.io.direction"]);

/// One read of the process, as the platform reported it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ProcessReading {
    /// Resident set size in bytes.
    pub(crate) resident_bytes: Option<u64>,
    /// Virtual memory size in bytes.
    pub(crate) virtual_bytes: Option<u64>,
    /// CPU time consumed since the process started, in milliseconds.
    pub(crate) cpu_time_ms: Option<u64>,
    /// CPU usage since the previous refresh, in percent of one core.
    pub(crate) cpu_percent: Option<f32>,
    /// Open file descriptors, or open handles on Windows.
    pub(crate) open_files: Option<u64>,
    /// Bytes read since the process started.
    pub(crate) read_bytes: Option<u64>,
    /// Bytes written since the process started.
    pub(crate) written_bytes: Option<u64>,
}

/// One published sample: the reading, when it was taken, and its changes since the
/// previous sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ProcessSample {
    /// Milliseconds since the Unix epoch, on the log records' clock.
    recorded_at_ms: i64,
    reading: ProcessReading,
    cpu_percent: Option<f64>,
    resident_change: Option<i64>,
    cpu_time_change_ms: Option<u64>,
    read_change: Option<u64>,
    written_change: Option<u64>,
}

impl ProcessSample {
    /// CPU usage in percent of one core, absent until the second refresh at least
    /// [`PROCESS_SAMPLE_INTERVAL_MIN`] after the first.
    pub(crate) fn cpu_percent(&self) -> Option<f64> {
        self.cpu_percent
    }

    /// When the sample was taken, in milliseconds since the Unix epoch.
    pub(crate) const fn recorded_at_ms(&self) -> i64 {
        self.recorded_at_ms
    }

    /// Resident set size in bytes.
    pub(crate) fn resident_bytes(&self) -> Option<u64> {
        self.reading.resident_bytes
    }

    /// Signed change of the resident set since the previous sample, in bytes.
    #[cfg(test)]
    pub(crate) const fn resident_change(&self) -> Option<i64> {
        self.resident_change
    }

    /// CPU time consumed since the previous sample, in milliseconds.
    #[cfg(test)]
    pub(crate) const fn cpu_time_change_ms(&self) -> Option<u64> {
        self.cpu_time_change_ms
    }

    /// Records the sample into every process instrument held in `values`.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a CPU time change past 2^53 milliseconds is not a sampler interval"
    )]
    pub(crate) fn record_into(&self, values: &MetricValues) {
        let metrics = metrics();
        metrics.memory.record_sample_into(values, self);
        metrics.cpu.record_sample_into(values, self);
        if let Some(bytes) = self.reading.virtual_bytes {
            PROCESS_MEMORY_VIRTUAL.record_into(values, [], bytes);
        }
        if let Some(count) = self.reading.open_files {
            PROCESS_OPEN_FILES.record_into(values, [], count);
        }
        if let Some(change) = self.cpu_time_change_ms {
            PROCESS_CPU_TIME.add_into(values, [], change as f64 / 1_000.0);
        }
        for (direction, change) in [("read", self.read_change), ("write", self.written_change)] {
            if let Some(bytes) = change {
                PROCESS_DISK_IO.add_into(values, [direction], bytes as f64);
            }
        }
    }
}

/// The previous reading the next sample's changes are taken against.
#[derive(Debug, Default)]
pub(crate) struct SampleSeries {
    previous: Option<(ProcessReading, Duration)>,
}

impl SampleSeries {
    /// The sample of `reading`, taken `at` on a monotonic clock and `recorded_at_ms` on
    /// the log clock.
    ///
    /// CPU usage is kept only when a previous reading exists at least
    /// [`PROCESS_SAMPLE_INTERVAL_MIN`] earlier: the first refresh reports no usage, and
    /// two readings at one instant measure none. A total that went backwards gives no
    /// change; the next sample measures from the lower total.
    pub(crate) fn observe(
        &mut self,
        reading: ProcessReading,
        at: Duration,
        recorded_at_ms: i64,
    ) -> ProcessSample {
        let previous = self.previous.replace((reading, at));
        let warmed = previous
            .is_some_and(|(_, then)| at.saturating_sub(then) >= PROCESS_SAMPLE_INTERVAL_MIN);
        let before = previous.map(|(before, _)| before).unwrap_or_default();
        ProcessSample {
            recorded_at_ms,
            reading,
            cpu_percent: reading
                .cpu_percent
                .filter(|_| warmed)
                .map(f64::from)
                .filter(|percent| percent.is_finite() && *percent >= 0.0),
            resident_change: signed_change(before.resident_bytes, reading.resident_bytes),
            cpu_time_change_ms: growth(before.cpu_time_ms, reading.cpu_time_ms),
            read_change: growth(before.read_bytes, reading.read_bytes),
            written_change: growth(before.written_bytes, reading.written_bytes),
        }
    }
}

/// How much a running total grew between two readings; absent when either reading is,
/// or when the total went backwards.
fn growth(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    after?.checked_sub(before?)
}

/// The signed change between two readings, absent when either is or the change does not
/// fit an `i64`.
fn signed_change(before: Option<u64>, after: Option<u64>) -> Option<i64> {
    let after = i128::from(after?);
    let before = i128::from(before?);
    i64::try_from(after - before).ok()
}

/// One read of the process the sampler watches.
pub(crate) trait ProcessReader: Send + 'static {
    /// Refreshes and reads the process. The call blocks on the OS.
    fn read(&mut self) -> ProcessReading;
}

/// Reads the current process through `sysinfo`.
pub(crate) struct SystemProcessReader {
    system: sysinfo::System,
    pid: Option<sysinfo::Pid>,
}

impl SystemProcessReader {
    /// A reader of the current process. A platform that names no current process reads
    /// nothing.
    pub(crate) fn current() -> Self {
        Self {
            system: sysinfo::System::new(),
            pid: sysinfo::get_current_pid().ok(),
        }
    }
}

impl ProcessReader for SystemProcessReader {
    fn read(&mut self) -> ProcessReading {
        let Some(pid) = self.pid else {
            return ProcessReading::default();
        };
        let refresh = sysinfo::ProcessRefreshKind::nothing()
            .with_memory()
            .with_cpu()
            .with_disk_usage()
            .without_tasks();
        self.system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            false,
            refresh,
        );
        let Some(process) = self.system.process(pid) else {
            return ProcessReading::default();
        };
        let disk = process.disk_usage();
        ProcessReading {
            resident_bytes: Some(process.memory()),
            virtual_bytes: Some(process.virtual_memory()),
            cpu_time_ms: Some(process.accumulated_cpu_time()),
            cpu_percent: Some(process.cpu_usage()),
            open_files: process
                .open_files()
                .and_then(|count| u64::try_from(count).ok()),
            read_bytes: Some(disk.total_read_bytes),
            written_bytes: Some(disk.total_written_bytes),
        }
    }
}

/// What the sampler publishes into the log stream beside each sample: the entries of the
/// table of operations in flight open past `stall_delay`, and metric snapshot records.
#[derive(Debug, Default)]
pub(crate) struct TickEvidence {
    /// The table the stall report reads; no table reports no stall.
    pub(crate) flights: Option<Arc<FlightTable>>,
    /// Age past which an entry is reported once; none reports no stall.
    pub(crate) stall_delay: Option<Duration>,
}

impl TickEvidence {
    /// Reports the stalled entries, then the metric snapshot of `values`: a snapshot
    /// publishes when a counter moved, or when this tick reported a stall.
    fn publish(&self, values: &MetricValues, snapshots: &mut SnapshotSeries) {
        let stalled = match (&self.flights, self.stall_delay) {
            (Some(flights), Some(stall_delay)) => {
                publish_stalled(flights, monotonic_now(), stall_delay)
            }
            _ => false,
        };
        snapshot::publish(&snapshots.records(&values.snapshot(), stalled));
    }
}

/// The running sampler: the task that ticks, and the token that stops it.
#[derive(Debug)]
pub(crate) struct ProcessSampler {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl ProcessSampler {
    /// Starts sampling the process `reader` reads every `interval` on the current Tokio
    /// runtime, publishing each sample into `values`. An interval below
    /// [`PROCESS_SAMPLE_INTERVAL_MIN`] samples at that minimum.
    pub(crate) fn spawn(
        reader: impl ProcessReader,
        interval: Duration,
        values: Arc<MetricValues>,
        evidence: TickEvidence,
    ) -> Self {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(sample(
            reader,
            interval.max(PROCESS_SAMPLE_INTERVAL_MIN),
            values,
            evidence,
            cancel.clone(),
        ));
        Self { cancel, task }
    }

    /// Stops ticking. A refresh already running on the blocking pool cannot be
    /// interrupted; it finishes on its own and its sample is not published.
    pub(crate) fn stop(self) {
        self.cancel.cancel();
        self.task.abort();
    }

    /// Stops ticking and waits for the task to end.
    #[cfg(test)]
    pub(crate) async fn stopped(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// Ticks every `interval` until `cancel` fires: one read on the blocking pool, one
/// published sample, then the tick's evidence. A read that panics ends the sampling; the
/// samples published before it stay.
async fn sample(
    mut reader: impl ProcessReader,
    interval: Duration,
    values: Arc<MetricValues>,
    evidence: TickEvidence,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let mut series = SampleSeries::default();
    let mut snapshots = SnapshotSeries::default();
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            _ = ticks.tick() => {}
        }
        let read = tokio::task::spawn_blocking(move || {
            let reading = reader.read();
            (reader, reading)
        });
        let joined = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            joined = read => joined,
        };
        let Ok((returned, reading)) = joined else {
            return;
        };
        reader = returned;
        let sample = series.observe(reading, started.elapsed(), now_ms());
        sample.record_into(&values);
        values.publish_sample(sample);
        evidence.publish(&values, &mut snapshots);
    }
}

#[cfg(test)]
mod tests;
