//! The process sampler: one refresh of the current process per tick, published as a sample.
//!
//! A tick reads the process once on the blocking pool, through `sysinfo`, and never walks
//! another process. The sample it publishes carries what the platform reported and the
//! changes since the previous one; a reading the platform does not report stays absent and
//! never becomes zero. The sampler records each sample into the process instruments, and
//! [`ProcessGauge::current`](crate::ProcessGauge::current) reads the latest one without an
//! OS read of its own.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

mod tokio_runtime;

use tokio::runtime::RuntimeMetrics;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use self::tokio_runtime::{RuntimeReading, RuntimeSeries};
use crate::capture::now_ms;
use crate::flight::{FlightTable, publish_stalled};
use crate::measurement::monotonic_now;
use crate::metrics::{Counter, Gauge, MetricLayer, MetricValues, metrics};
use crate::snapshot::{self, SnapshotSeries};

/// The shortest interval the sampler refreshes the process at. CPU usage is the change of
/// CPU time over the wall time between two refreshes, and `sysinfo` reads it reliably only
/// with at least its `MINIMUM_CPU_UPDATE_INTERVAL` between them, 200 ms on Linux, macOS,
/// and Windows; a shorter interval samples at this one.
pub const PROCESS_SAMPLE_INTERVAL_MIN: Duration = Duration::from_millis(200);
const _: () = assert!(
    sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.as_millis() <= PROCESS_SAMPLE_INTERVAL_MIN.as_millis()
);

/// Sample hooks one dispatcher's sampler runs, at most. A registration past it is refused.
pub const SAMPLE_HOOKS_MAX: usize = 64;

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

/// The callback a [`SampleHook`] hands the sampler.
type SampleRead = dyn Fn() + Send + Sync;

/// A callback the process sampler runs on each tick for as long as this value lives:
/// dropping it removes the callback.
///
/// The callback runs on the blocking pool beside the process read, under the dispatcher
/// that was current when it was registered, so the gauges it records land in that
/// dispatcher's metric values before the tick's snapshot reads them. It may block on a
/// file metadata read; it holds up its own tick and no runtime worker. A callback that
/// panics loses that tick's reads and stays registered.
#[must_use = "dropping the hook removes it from the sampler"]
pub struct SampleHook {
    _read: Arc<SampleRead>,
}

impl fmt::Debug for SampleHook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SampleHook").finish_non_exhaustive()
    }
}

/// Registers `read` with the sampler of the thread's current dispatcher, to run on each
/// tick until the returned [`SampleHook`] drops.
///
/// Answers `None`, and registers nothing, when the dispatcher holds no metric values, or
/// when [`SAMPLE_HOOKS_MAX`] hooks are already registered with it. A dispatcher whose
/// runtime runs no sampler holds the hook and never runs it.
///
/// ```
/// const QUEUED: rift_tracing::Gauge<u64, 0> =
///     rift_tracing::Gauge::declare("test.queue.length", "{task}", &[]);
/// let hook = rift_tracing::sample_hook(|| QUEUED.value(3).record());
/// assert!(hook.is_none(), "no dispatcher holds metric values here");
/// ```
pub fn sample_hook(read: impl Fn() + Send + Sync + 'static) -> Option<SampleHook> {
    let mut read = Some(read);
    tracing::dispatcher::get_default(|dispatch| {
        let layer = dispatch.downcast_ref::<MetricLayer>()?;
        let read: Arc<SampleRead> = Arc::new(read.take()?);
        layer
            .values()
            .hooks()
            .register(&read, dispatch.downgrade())
            .then_some(SampleHook { _read: read })
    })
}

/// The hooks one dispatcher's metric values hold: each callback and its dispatcher, both
/// weak, so neither a dropped owner's callback nor the dispatcher stays alive through them.
#[derive(Default)]
pub(crate) struct SampleHooks {
    registered: Mutex<Vec<(Weak<SampleRead>, tracing::dispatcher::WeakDispatch)>>,
}

impl fmt::Debug for SampleHooks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SampleHooks")
            .field("registered", &self.lock().len())
            .finish()
    }
}

impl SampleHooks {
    fn lock(&self) -> MutexGuard<'_, Vec<(Weak<SampleRead>, tracing::dispatcher::WeakDispatch)>> {
        self.registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds `read`, recording through `dispatch`, unless [`SAMPLE_HOOKS_MAX`] live hooks
    /// are registered; answers whether it did. Hooks whose owner or dispatcher dropped
    /// leave first.
    fn register(
        &self,
        read: &Arc<SampleRead>,
        dispatch: tracing::dispatcher::WeakDispatch,
    ) -> bool {
        let mut registered = self.lock();
        registered
            .retain(|(read, dispatch)| read.strong_count() > 0 && dispatch.upgrade().is_some());
        if registered.len() >= SAMPLE_HOOKS_MAX {
            return false;
        }
        registered.push((Arc::downgrade(read), dispatch));
        true
    }

    /// The hooks whose owner and dispatcher are still alive; the rest leave.
    pub(crate) fn live(&self) -> Vec<(Arc<SampleRead>, tracing::Dispatch)> {
        let mut live = Vec::new();
        self.lock().retain(|(read, dispatch)| {
            let Some(hook) = read.upgrade().zip(dispatch.upgrade()) else {
                return false;
            };
            live.push(hook);
            true
        });
        live
    }

    /// The count of hooks registered, those of dropped owners included until they leave.
    #[cfg(test)]
    pub(crate) fn registered(&self) -> usize {
        self.lock().len()
    }
}

/// Runs each of `hooks` under its dispatcher, and answers how many it ran. A panic ends
/// that hook's run alone.
pub(crate) fn run_hooks(hooks: Vec<(Arc<SampleRead>, tracing::Dispatch)>) -> usize {
    let count = hooks.len();
    for (read, dispatch) in hooks {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            tracing::dispatcher::with_default(&dispatch, || read());
        }));
    }
    count
}

/// What the sampler publishes into the log stream beside each sample: the runtime sample,
/// the entries of the table of operations in flight open past `stall_delay`, and metric
/// snapshot records.
#[derive(Debug, Default)]
pub(crate) struct TickEvidence {
    /// The Tokio runtime each tick reads into the `runtime` group; none reads no runtime.
    pub(crate) runtime: Option<RuntimeMetrics>,
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

/// Ticks every `interval` until `cancel` fires: one read on the blocking pool, followed
/// there by every live [`SampleHook`], one published sample, then the tick's evidence. A
/// read that panics ends the sampling; the samples published before it stay.
async fn sample(
    mut reader: impl ProcessReader,
    interval: Duration,
    values: Arc<MetricValues>,
    evidence: TickEvidence,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let mut series = SampleSeries::default();
    let mut runtime = RuntimeSeries::default();
    let mut snapshots = SnapshotSeries::default();
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            _ = ticks.tick() => {}
        }
        let hooks = values.hooks().live();
        let read = tokio::task::spawn_blocking(move || {
            let reading = reader.read();
            let _ran = run_hooks(hooks);
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
        if let Some(metrics) = &evidence.runtime {
            runtime.observe(RuntimeReading::of(metrics), &values);
        }
        evidence.publish(&values, &mut snapshots);
    }
}

#[cfg(test)]
mod tests;
