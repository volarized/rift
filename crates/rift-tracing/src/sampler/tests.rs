use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::{
    PROCESS_SAMPLE_INTERVAL_MIN, ProcessReader, ProcessReading, ProcessSampler, RuntimeReading,
    RuntimeSeries, SampleSeries, SystemProcessReader, TickEvidence,
};
use crate::RecordKind;
use crate::flight::{FlightEntry, FlightKind, FlightTable};
use crate::metrics::{MetricValues, SeriesValue};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A reading with every value the platforms report.
fn reading(
    resident: u64,
    cpu_time_ms: u64,
    cpu_percent: f32,
    read: u64,
    written: u64,
) -> ProcessReading {
    ProcessReading {
        resident_bytes: Some(resident),
        virtual_bytes: Some(resident * 4),
        cpu_time_ms: Some(cpu_time_ms),
        cpu_percent: Some(cpu_percent),
        open_files: Some(12),
        read_bytes: Some(read),
        written_bytes: Some(written),
    }
}

#[test]
fn the_first_sample_measures_no_cpu_usage_and_no_change() {
    let mut series = SampleSeries::default();
    let sample = series.observe(reading(100, 50, 30.0, 10, 20), Duration::ZERO, 7);
    assert_eq!(sample.cpu_percent(), None, "CPU usage needs two refreshes");
    assert_eq!(sample.resident_bytes(), Some(100));
    assert_eq!(sample.resident_change(), None);
    assert_eq!(sample.cpu_time_change_ms(), None);
    assert_eq!(sample.recorded_at_ms(), 7);
}

#[test]
fn cpu_usage_counts_from_the_second_refresh_one_interval_later_and_can_pass_100() {
    let mut series = SampleSeries::default();
    let _ = series.observe(reading(100, 50, 0.0, 10, 20), Duration::ZERO, 0);
    let early = series.observe(
        reading(100, 60, 80.0, 10, 20),
        Duration::from_millis(199),
        1,
    );
    assert_eq!(
        early.cpu_percent(),
        None,
        "a refresh too soon after the last measures none"
    );
    let warmed = series.observe(
        reading(100, 460, 250.0, 10, 20),
        PROCESS_SAMPLE_INTERVAL_MIN * 2,
        2,
    );
    assert_eq!(
        warmed.cpu_percent(),
        Some(250.0),
        "three cores busy read 250 percent"
    );
    assert_eq!(warmed.cpu_time_change_ms(), Some(400));
}

#[test]
fn two_readings_at_one_instant_measure_no_cpu_usage() {
    let mut series = SampleSeries::default();
    let at = Duration::from_secs(5);
    let _ = series.observe(reading(100, 50, 10.0, 10, 20), at, 0);
    let same = series.observe(reading(100, 50, 10.0, 10, 20), at, 0);
    assert_eq!(same.cpu_percent(), None);
    assert_eq!(same.cpu_time_change_ms(), Some(0));
}

#[test]
fn a_total_that_went_backwards_gives_no_change_and_the_next_counts_from_it() {
    let mut series = SampleSeries::default();
    let _ = series.observe(reading(100, 500, 0.0, 1_000, 2_000), Duration::ZERO, 0);
    let regressed = series.observe(
        reading(100, 400, 1.0, 900, 2_500),
        Duration::from_secs(1),
        1,
    );
    assert_eq!(regressed.cpu_time_change_ms(), None);
    assert_eq!(regressed.read_change, None);
    assert_eq!(regressed.written_change, Some(500));
    let next = series.observe(
        reading(100, 450, 1.0, 950, 2_500),
        Duration::from_secs(2),
        2,
    );
    assert_eq!(next.cpu_time_change_ms(), Some(50));
    assert_eq!(next.read_change, Some(50));
}

#[test]
fn a_shrinking_resident_set_is_a_negative_change() {
    let mut series = SampleSeries::default();
    let _ = series.observe(reading(4_096, 0, 0.0, 0, 0), Duration::ZERO, 0);
    let shrunk = series.observe(reading(1_024, 0, 0.0, 0, 0), Duration::from_secs(1), 1);
    assert_eq!(shrunk.resident_change(), Some(-3_072));
}

#[test]
fn a_missing_reading_stays_absent_and_is_never_zero() {
    let mut series = SampleSeries::default();
    let _ = series.observe(reading(100, 50, 0.0, 10, 20), Duration::ZERO, 0);
    let missing = series.observe(ProcessReading::default(), Duration::from_secs(1), 1);
    assert_eq!(missing.resident_bytes(), None);
    assert_eq!(missing.resident_change(), None);
    assert_eq!(missing.cpu_percent(), None);
    assert_eq!(missing.cpu_time_change_ms(), None);

    let values = MetricValues::default();
    missing.record_into(&values);
    assert!(
        values.snapshot().series().is_empty(),
        "no reading, no series"
    );
}

#[test]
fn a_sample_records_every_process_instrument_it_measured() {
    let mut series = SampleSeries::default();
    let values = MetricValues::default();
    series
        .observe(reading(1_000, 1_000, 0.0, 100, 200), Duration::ZERO, 0)
        .record_into(&values);
    series
        .observe(
            reading(3_000, 2_500, 150.0, 400, 200),
            Duration::from_secs(1),
            1,
        )
        .record_into(&values);

    let snapshot = values.snapshot();
    let value = |name: &str, labels: &[(&str, &str)]| {
        snapshot
            .find(name, labels)
            .map(|series| series.value().clone())
    };
    assert_eq!(
        value("process.memory.usage", &[]),
        Some(SeriesValue::Last(3_000.0))
    );
    assert_eq!(
        value("process.memory.virtual", &[]),
        Some(SeriesValue::Last(12_000.0))
    );
    assert_eq!(
        value("process.cpu.utilization", &[]),
        Some(SeriesValue::Last(1.5))
    );
    assert_eq!(value("process.cpu.time", &[]), Some(SeriesValue::Sum(1.5)));
    assert_eq!(
        value("process.disk.io", &[("disk.io.direction", "read")]),
        Some(SeriesValue::Sum(300.0))
    );
    assert_eq!(
        value("process.disk.io", &[("disk.io.direction", "write")]),
        Some(SeriesValue::Sum(0.0))
    );
    let open_files = if cfg!(windows) {
        "process.windows.handle.count"
    } else {
        "process.unix.file_descriptor.count"
    };
    assert_eq!(value(open_files, &[]), Some(SeriesValue::Last(12.0)));
}

#[test]
fn the_system_reader_reads_the_current_process() {
    let mut reader = SystemProcessReader::current();
    let reading = reader.read();
    assert!(
        reading.resident_bytes.is_some_and(|bytes| bytes > 0),
        "a running process holds resident memory: {reading:?}"
    );
    assert!(reading.cpu_time_ms.is_some());
}

/// Hands each read's ordinal to the test, and reads `resident_bytes` as that ordinal.
struct CountingReader {
    reads: u64,
    sent: mpsc::UnboundedSender<u64>,
}

impl ProcessReader for CountingReader {
    fn read(&mut self) -> ProcessReading {
        self.reads += 1;
        let _ = self.sent.send(self.reads);
        ProcessReading {
            resident_bytes: Some(self.reads),
            ..ProcessReading::default()
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_sampler_publishes_one_sample_per_tick_until_it_stops() {
    let (sent, mut reads) = mpsc::unbounded_channel();
    let values = Arc::new(MetricValues::default());
    let reader = CountingReader { reads: 0, sent };
    let sampler = ProcessSampler::spawn(
        reader,
        Duration::from_secs(1),
        Arc::clone(&values),
        TickEvidence::default(),
    );

    for expected in 1..=3 {
        let read = tokio::time::timeout(Duration::from_secs(5), reads.recv())
            .await
            .expect("the sampler reads once per tick");
        assert_eq!(read, Some(expected));
    }
    sampler.stopped().await;
    assert!(
        values
            .latest_sample()
            .and_then(|sample| sample.resident_bytes())
            .is_some_and(|ordinal| ordinal >= 2),
        "the samples before the stop were published"
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(
        reads.try_recv().is_err(),
        "a stopped sampler reads nothing more"
    );
}

#[tokio::test(start_paused = true)]
async fn an_interval_below_the_minimum_samples_at_the_minimum() {
    let (sent, mut reads) = mpsc::unbounded_channel();
    let values = Arc::new(MetricValues::default());
    let reader = CountingReader { reads: 0, sent };
    let started = tokio::time::Instant::now();
    let sampler = ProcessSampler::spawn(
        reader,
        Duration::from_millis(1),
        values,
        TickEvidence::default(),
    );
    assert_eq!(reads.recv().await, Some(1), "the first tick reads at once");
    assert_eq!(reads.recv().await, Some(2));
    assert!(started.elapsed() >= PROCESS_SAMPLE_INTERVAL_MIN);
    sampler.stopped().await;
}

#[tokio::test(start_paused = true)]
async fn a_tick_reports_an_entry_past_the_stall_delay_once_with_a_snapshot() -> TestResult {
    let (recorder, mut drain) = crate::ScopedRecorder::builder().install()?;
    let flights = Arc::new(FlightTable::default());
    flights.join(
        1,
        FlightEntry::opened(
            "lexical.commit",
            FlightKind::Operation,
            Some("search.request"),
            Duration::ZERO,
            0,
        ),
    );
    let (sent, mut reads) = mpsc::unbounded_channel();
    let values = Arc::new(MetricValues::default());
    let reader = CountingReader { reads: 0, sent };
    let evidence = TickEvidence {
        runtime: None,
        flights: Some(Arc::clone(&flights)),
        stall_delay: Some(Duration::ZERO),
    };
    let sampler = ProcessSampler::spawn(reader, Duration::from_secs(1), values, evidence);
    for expected in 1..=2 {
        assert_eq!(reads.recv().await, Some(expected));
    }
    sampler.stopped().await;
    drop(recorder);

    let records = drain.queued_records();
    let stalled: Vec<_> = records
        .iter()
        .filter(|record| record.message() == "operations in flight past the stall delay")
        .collect();
    assert_eq!(stalled.len(), 1, "an entry is reported once");
    assert_eq!(stalled[0].level(), "warn");
    let fields: serde_json::Value = serde_json::from_str(stalled[0].fields())?;
    assert_eq!(fields["reason"], "stall_delay");
    assert!(stalled[0].fields().contains("lexical.commit"));
    let snapshots: Vec<_> = records
        .iter()
        .filter(|record| record.kind() == RecordKind::Metric)
        .collect();
    assert_eq!(
        snapshots.len(),
        1,
        "the stall tick forces one snapshot, the idle one none"
    );
    assert_eq!(snapshots[0].operation(), "process");
    Ok(())
}

/// A lifelong hold open past the stall delay draws no stall report and no forced
/// snapshot; an operation past the delay beside it is reported alone.
#[tokio::test(start_paused = true)]
async fn a_lifelong_hold_past_the_stall_delay_is_never_reported() -> TestResult {
    let (recorder, mut drain) = crate::ScopedRecorder::builder().install()?;
    let flights = Arc::new(FlightTable::default());
    for (identity, lock) in [(1, "history.live"), (2, "history.fill")] {
        let mut held = FlightEntry::opened(
            "lock.held",
            FlightKind::Held,
            Some("history.open"),
            Duration::ZERO,
            0,
        );
        held.lock = lock.to_owned();
        held.lifelong = true;
        flights.join(identity, held);
    }
    let (sent, mut reads) = mpsc::unbounded_channel();
    let reader = CountingReader { reads: 0, sent };
    let evidence = TickEvidence {
        runtime: None,
        flights: Some(Arc::clone(&flights)),
        stall_delay: Some(Duration::ZERO),
    };
    let sampler = ProcessSampler::spawn(
        reader,
        Duration::from_secs(1),
        Arc::new(MetricValues::default()),
        evidence,
    );
    for expected in 1..=2 {
        assert_eq!(reads.recv().await, Some(expected));
    }
    flights.join(
        3,
        FlightEntry::opened(
            "lexical.commit",
            FlightKind::Operation,
            Some("search.request"),
            Duration::ZERO,
            0,
        ),
    );
    assert_eq!(reads.recv().await, Some(3));
    assert_eq!(reads.recv().await, Some(4), "the third tick published");
    sampler.stopped().await;
    drop(recorder);

    let records = drain.queued_records();
    let stalled: Vec<_> = records
        .iter()
        .filter(|record| record.message() == "operations in flight past the stall delay")
        .collect();
    assert_eq!(stalled.len(), 1, "only the operation draws a report");
    let fields: serde_json::Value = serde_json::from_str(stalled[0].fields())?;
    assert_eq!(fields["in_flight"], "1");
    let listed: serde_json::Value =
        serde_json::from_str(fields["operations"].as_str().ok_or("operations is text")?)?;
    assert_eq!(listed[0]["operation"], "lexical.commit");
    assert!(!stalled[0].fields().contains("history.live"));
    Ok(())
}

/// The value of the series `name` without labels in `values`.
fn unlabeled(values: &MetricValues, name: &str) -> Option<SeriesValue> {
    values
        .snapshot()
        .find(name, &[])
        .map(|series| series.value().clone())
}

/// A runtime reading of two workers.
fn runtime_reading(
    busy_ms: [u64; 2],
    parks: [u64; 2],
    global_queue_depth: usize,
) -> RuntimeReading {
    RuntimeReading {
        workers: 2,
        alive_tasks: 7,
        global_queue_depth,
        busy: busy_ms.map(Duration::from_millis).to_vec(),
        parks: parks.to_vec(),
    }
}

#[test]
fn the_first_runtime_reading_records_its_counts_and_no_change() {
    let values = MetricValues::default();
    let mut series = RuntimeSeries::default();
    series.observe(runtime_reading([500, 900], [3, 4], 0), &values);

    assert_eq!(
        unlabeled(&values, "tokio.runtime.worker.count"),
        Some(SeriesValue::Last(2.0))
    );
    assert_eq!(
        unlabeled(&values, "tokio.runtime.task.count"),
        Some(SeriesValue::Last(7.0))
    );
    assert_eq!(
        unlabeled(&values, "tokio.runtime.global_queue.length"),
        Some(SeriesValue::Last(0.0))
    );
    for name in [
        "tokio.runtime.worker.busy.time",
        "tokio.runtime.worker.busy.time.max",
        "tokio.runtime.worker.parks",
    ] {
        assert_eq!(unlabeled(&values, name), None, "{name} needs a base first");
    }
}

#[test]
fn a_runtime_reading_records_busy_time_and_parks_since_the_previous_one() {
    let values = MetricValues::default();
    let mut series = RuntimeSeries::default();
    series.observe(runtime_reading([500, 900], [3, 4], 0), &values);
    series.observe(runtime_reading([1_500, 1_150], [3, 9], 12), &values);

    assert_eq!(
        unlabeled(&values, "tokio.runtime.worker.busy.time"),
        Some(SeriesValue::Sum(1.25)),
        "1 s on one worker and 0.25 s on the other"
    );
    assert_eq!(
        unlabeled(&values, "tokio.runtime.worker.busy.time.max"),
        Some(SeriesValue::Last(1.0)),
        "the busiest worker filled the interval alone"
    );
    assert_eq!(
        unlabeled(&values, "tokio.runtime.worker.parks"),
        Some(SeriesValue::Sum(5.0))
    );
    assert_eq!(
        unlabeled(&values, "tokio.runtime.global_queue.length"),
        Some(SeriesValue::Last(12.0))
    );
}

#[test]
fn a_runtime_total_that_went_backwards_adds_nothing() {
    let values = MetricValues::default();
    let mut series = RuntimeSeries::default();
    series.observe(runtime_reading([500, 900], [3, 4], 0), &values);
    series.observe(runtime_reading([400, 900], [2, 4], 0), &values);

    assert_eq!(
        unlabeled(&values, "tokio.runtime.worker.busy.time"),
        Some(SeriesValue::Sum(0.0))
    );
    assert_eq!(
        unlabeled(&values, "tokio.runtime.worker.parks"),
        Some(SeriesValue::Sum(0.0))
    );
}

#[tokio::test]
async fn a_runtime_reading_reads_the_runtime_it_runs_on() {
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let waiting = tokio::spawn(released);
    let reading = RuntimeReading::of(&tokio::runtime::Handle::current().metrics());
    let _ = release.send(());
    let _ = waiting.await;

    assert_eq!(
        reading.workers, 1,
        "a current-thread runtime has one worker"
    );
    assert_eq!(
        reading.alive_tasks, 1,
        "the spawned task is alive: {reading:?}"
    );
    if cfg!(target_has_atomic = "64") {
        assert_eq!(reading.busy.len(), 1);
        assert_eq!(reading.parks.len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_tick_reads_the_runtime_into_the_runtime_group() {
    let (sent, mut reads) = mpsc::unbounded_channel();
    let values = Arc::new(MetricValues::default());
    let reader = CountingReader { reads: 0, sent };
    let evidence = TickEvidence {
        runtime: Some(tokio::runtime::Handle::current().metrics()),
        ..TickEvidence::default()
    };
    let sampler = ProcessSampler::spawn(
        reader,
        Duration::from_secs(1),
        Arc::clone(&values),
        evidence,
    );
    for expected in 1..=2 {
        assert_eq!(reads.recv().await, Some(expected));
    }
    sampler.stopped().await;

    assert_eq!(
        unlabeled(&values, "tokio.runtime.worker.count"),
        Some(SeriesValue::Last(1.0)),
        "the first tick read the runtime before the second read began"
    );
}

/// A dispatcher over `values` alone, as a runtime's carries them.
fn dispatch_over(values: &Arc<MetricValues>) -> tracing::Dispatch {
    use tracing_subscriber::layer::SubscriberExt;

    tracing::Dispatch::new(
        tracing_subscriber::registry().with(crate::metrics::MetricLayer::new(Arc::clone(values))),
    )
}

/// A hook runs on each tick and records into the values of the dispatcher it was
/// registered under; once its owner drops it, it runs at most the tick already started and
/// then leaves.
#[tokio::test(start_paused = true)]
async fn a_sample_hook_runs_on_each_tick_until_its_owner_drops() -> TestResult {
    use std::sync::atomic::{AtomicU64, Ordering};

    const HOOKED: crate::Gauge<u64, 0> = crate::Gauge::declare("test.hooked", "{run}", &[]);
    let values = Arc::new(MetricValues::default());
    let runs = Arc::new(AtomicU64::new(0));
    let dispatch = dispatch_over(&values);
    let hook = tracing::dispatcher::with_default(&dispatch, || {
        let runs = Arc::clone(&runs);
        crate::sample_hook(move || {
            let run = runs.fetch_add(1, Ordering::SeqCst) + 1;
            HOOKED.value(run).record();
        })
    })
    .ok_or("the dispatcher holds metric values")?;
    let (sent, mut reads) = mpsc::unbounded_channel();
    let sampler = ProcessSampler::spawn(
        CountingReader { reads: 0, sent },
        Duration::from_secs(1),
        Arc::clone(&values),
        TickEvidence::default(),
    );
    for expected in 1..=3 {
        assert_eq!(reads.recv().await, Some(expected));
    }
    let before_drop = runs.load(Ordering::SeqCst);
    assert!(before_drop >= 2, "two ticks finished: {before_drop}");
    assert_eq!(
        unlabeled(&values, "test.hooked"),
        Some(SeriesValue::Last(f64::from(u32::try_from(before_drop)?)))
    );
    drop(hook);
    assert_eq!(reads.recv().await, Some(4));
    let settled = runs.load(Ordering::SeqCst);
    assert!(settled <= 3, "only the tick already started ran: {settled}");
    assert_eq!(reads.recv().await, Some(5));
    assert_eq!(reads.recv().await, Some(6));
    sampler.stopped().await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        settled,
        "no run after the owner dropped"
    );
    assert_eq!(values.hooks().registered(), 0, "the dropped hook left");
    Ok(())
}

/// A dispatcher keeps at most `SAMPLE_HOOKS_MAX` hooks: one more is refused until an owner
/// drops its own.
#[test]
fn sample_hooks_past_the_bound_are_refused_until_one_drops() -> TestResult {
    let values = Arc::new(MetricValues::default());
    let dispatch = dispatch_over(&values);
    tracing::dispatcher::with_default(&dispatch, || {
        let mut hooks: Vec<_> = (0..crate::SAMPLE_HOOKS_MAX)
            .map(|_| crate::sample_hook(|| {}))
            .collect::<Option<_>>()
            .ok_or("every hook within the bound registers")?;
        assert!(crate::sample_hook(|| {}).is_none(), "one past the bound");
        hooks.pop();
        let again = crate::sample_hook(|| {});
        assert!(again.is_some(), "a dropped owner frees its place");
        assert_eq!(values.hooks().registered(), crate::SAMPLE_HOOKS_MAX);
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    assert!(
        crate::sample_hook(|| {}).is_none(),
        "a thread whose dispatcher holds no metric values registers nothing"
    );
    Ok(())
}

/// A hook that panics loses its own run; the sampler keeps ticking and the next hook runs.
#[tokio::test(start_paused = true)]
async fn a_panicking_sample_hook_leaves_the_sampler_ticking() -> TestResult {
    use std::sync::atomic::{AtomicU64, Ordering};

    let values = Arc::new(MetricValues::default());
    let runs = Arc::new(AtomicU64::new(0));
    let dispatch = dispatch_over(&values);
    let (panicking, counting) = tracing::dispatcher::with_default(&dispatch, || {
        let runs = Arc::clone(&runs);
        (
            crate::sample_hook(|| panic!("a hook that fails")),
            crate::sample_hook(move || {
                runs.fetch_add(1, Ordering::SeqCst);
            }),
        )
    });
    let (sent, mut reads) = mpsc::unbounded_channel();
    let sampler = ProcessSampler::spawn(
        CountingReader { reads: 0, sent },
        Duration::from_secs(1),
        Arc::clone(&values),
        TickEvidence::default(),
    );
    for expected in 1..=3 {
        assert_eq!(reads.recv().await, Some(expected));
    }
    sampler.stopped().await;
    assert!(
        runs.load(Ordering::SeqCst) >= 2,
        "the next hook ran on every tick"
    );
    drop((panicking, counting));
    Ok(())
}
