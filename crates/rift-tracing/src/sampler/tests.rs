use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::{
    PROCESS_SAMPLE_INTERVAL_MIN, ProcessReader, ProcessReading, ProcessSampler, SampleSeries,
    SystemProcessReader, TickEvidence,
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
