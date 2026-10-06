use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{ProcessReader, ProcessReading, SystemProcessReader, observe_process, observe_runtime};
use crate::{MetricSnapshot, ScopedRecorder, SeriesValue};

/// A recorder whose metric reads the assertions take.
fn recorder() -> ScopedRecorder {
    ScopedRecorder::builder()
        .install()
        .expect("the default capture filter parses")
        .0
}

/// The value of the series `name` whose labels are `labels`.
fn value(snapshot: &MetricSnapshot, name: &str, labels: &[(&str, &str)]) -> Option<SeriesValue> {
    snapshot
        .find(name, labels)
        .map(|series| series.value().clone())
}

/// The unit of the series `name` without labels.
fn unit(snapshot: &MetricSnapshot, name: &str) -> Option<String> {
    snapshot
        .find(name, &[])
        .map(|series| series.unit().to_owned())
}

/// Reads fixed values, and counts its reads.
struct FixedReader {
    reading: ProcessReading,
    reads: Arc<AtomicU64>,
}

impl ProcessReader for FixedReader {
    fn read(&mut self) -> ProcessReading {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.reading
    }
}

/// A reading with every value the platforms report.
const EVERY_VALUE: ProcessReading = ProcessReading {
    resident_bytes: Some(4_096),
    virtual_bytes: Some(16_384),
    cpu_time_ms: Some(1_500),
    open_files: Some(12),
    read_bytes: Some(700),
    written_bytes: Some(300),
};

#[cfg(not(windows))]
const OPEN_FILES: (&str, &str) = ("process.unix.file_descriptor.count", "{file_descriptor}");
#[cfg(windows)]
const OPEN_FILES: (&str, &str) = ("process.windows.handle.count", "{handle}");

/// Without a meter the readings would report nothing, and none registers or reads.
#[test]
fn a_process_without_a_meter_registers_no_reading() {
    let reads = Arc::new(AtomicU64::new(0));
    let reader = FixedReader {
        reading: EVERY_VALUE,
        reads: Arc::clone(&reads),
    };
    assert!(!observe_process(reader));
    assert_eq!(reads.load(Ordering::Relaxed), 0);
}

/// Each collection reads the process, and every reading lands under its name, unit, and
/// labels: the counts and sizes as up-down counters, the totals as counters.
#[test]
fn every_process_reading_reaches_the_meter_at_each_collection() {
    let recorder = recorder();
    let reads = Arc::new(AtomicU64::new(0));
    let reader = FixedReader {
        reading: EVERY_VALUE,
        reads: Arc::clone(&reads),
    };
    assert!(observe_process(reader));
    assert_eq!(
        reads.load(Ordering::Relaxed),
        0,
        "registration reads nothing"
    );

    let snapshot = recorder.metrics();
    let (open_files, files_unit) = OPEN_FILES;
    for (name, expected_unit, expected) in [
        ("process.memory.usage", "By", 4_096.0),
        ("process.memory.virtual", "By", 16_384.0),
        ("process.cpu.time", "s", 1.5),
        (open_files, files_unit, 12.0),
    ] {
        assert_eq!(
            value(&snapshot, name, &[]),
            Some(SeriesValue::Sum(expected)),
            "{name}"
        );
        assert_eq!(
            unit(&snapshot, name).as_deref(),
            Some(expected_unit),
            "{name}"
        );
    }
    for (direction, expected) in [("read", 700.0), ("write", 300.0)] {
        assert_eq!(
            value(
                &snapshot,
                "process.disk.io",
                &[("disk.io.direction", direction)]
            ),
            Some(SeriesValue::Sum(expected))
        );
    }
    assert_eq!(
        reads.load(Ordering::Relaxed),
        5,
        "one read per reading and collection"
    );
    let _again = recorder.metrics();
    assert_eq!(
        reads.load(Ordering::Relaxed),
        10,
        "the next collection reads anew"
    );
}

/// A value the platform does not report stays absent and never becomes zero.
#[test]
fn a_missing_reading_stays_absent_and_is_never_zero() {
    let recorder = recorder();
    let reader = FixedReader {
        reading: ProcessReading {
            resident_bytes: Some(4_096),
            ..ProcessReading::default()
        },
        reads: Arc::new(AtomicU64::new(0)),
    };
    assert!(observe_process(reader));
    let snapshot = recorder.metrics();
    assert_eq!(
        value(&snapshot, "process.memory.usage", &[]),
        Some(SeriesValue::Sum(4_096.0))
    );
    for name in ["process.memory.virtual", "process.cpu.time", OPEN_FILES.0] {
        assert_eq!(value(&snapshot, name, &[]), None, "{name}");
    }
    assert_eq!(
        value(
            &snapshot,
            "process.disk.io",
            &[("disk.io.direction", "read")]
        ),
        None
    );
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

/// The runtime readings report the runtime they read: one worker on a current-thread
/// runtime, the live task count, the global queue, and the totals the workers counted.
#[tokio::test]
async fn the_runtime_readings_report_the_runtime_they_read() {
    let recorder = recorder();
    assert!(observe_runtime(
        &tokio::runtime::Handle::current().metrics()
    ));
    let snapshot = recorder.metrics();
    assert_eq!(
        value(&snapshot, "tokio.runtime.worker.count", &[]),
        Some(SeriesValue::Sum(1.0))
    );
    assert_eq!(
        unit(&snapshot, "tokio.runtime.worker.count").as_deref(),
        Some("{thread}")
    );
    for (name, expected_unit) in [
        ("tokio.runtime.task.count", "{task}"),
        ("tokio.runtime.global_queue.length", "{task}"),
        #[cfg(target_has_atomic = "64")]
        ("tokio.runtime.worker.busy.time", "s"),
        #[cfg(target_has_atomic = "64")]
        ("tokio.runtime.worker.parks", "{park}"),
    ] {
        assert_eq!(
            unit(&snapshot, name).as_deref(),
            Some(expected_unit),
            "{name}"
        );
    }
}

/// Without a meter no runtime reading registers.
#[tokio::test]
async fn a_process_without_a_meter_registers_no_runtime_reading() {
    assert!(!observe_runtime(
        &tokio::runtime::Handle::current().metrics()
    ));
}
