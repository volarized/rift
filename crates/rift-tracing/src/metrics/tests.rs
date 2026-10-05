use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use super::{
    Counter, DURATION_BOUNDARIES_SECONDS, Gauge, Histogram, completion, meter_installed, metrics,
};
use crate::{MetricSnapshot, ScopedRecorder, SeriesValue};

static DROPS: Counter<1> = Counter::declare("log.queue.dropped", "{record}", &["error.type"]);
static PLAIN: Counter<0> = Counter::declare("test.plain", "{event}", &[]);
static QUEUE: Gauge<u64, 0> = Gauge::declare("test.queue.length", "{task}", &[]);
static RATIO: Gauge<f64, 0> = Gauge::declare("test.ratio", "1", &[]).scaled(0.01);
static WAIT: Histogram<0> = Histogram::declare("test.wait.duration", &[]);
static SHORT: Histogram<0> = Histogram::declare("test.short.duration", &[]).boundaries(&[0.1, 1.0]);
static STATEMENTS: Histogram<1, u64> = Histogram::declare_count(
    "test.statement.count",
    "{statement}",
    &["db.namespace"],
    &[1.0, 10.0],
);

fn recorder() -> ScopedRecorder {
    ScopedRecorder::builder()
        .install()
        .expect("the default capture filter parses")
        .0
}

fn value(snapshot: &MetricSnapshot, name: &str, labels: &[(&str, &str)]) -> Option<SeriesValue> {
    snapshot
        .find(name, labels)
        .map(|series| series.value().clone())
}

fn calls(snapshot: &MetricSnapshot, operation: &str, outcome: &[(&str, &str)]) -> Option<f64> {
    let mut labels = vec![("span.name", operation)];
    labels.extend_from_slice(outcome);
    match value(snapshot, "traces.span.metrics.calls", &labels)? {
        SeriesValue::Sum(sum) => Some(sum),
        other => panic!("a counter holds a sum, not {other:?}"),
    }
}

fn poll_once<Work: Future>(work: std::pin::Pin<&mut Work>) -> Poll<Work::Output> {
    work.poll(&mut Context::from_waker(Waker::noop()))
}

const OK: [(&str, &str); 1] = [("status.code", "Ok")];
const PANICKED: [(&str, &str); 2] = [("status.code", "Error"), ("error.type", "panic")];
const CANCELLED: [(&str, &str); 2] = [("status.code", "Error"), ("error.type", "cancelled")];

#[test]
fn counter_adds_into_the_series_its_labels_select() {
    let recorder = recorder();
    PLAIN.add(2);
    PLAIN.add(3);
    DROPS.labeled(["queue_full"]).add(4);
    DROPS.labeled(["unwritten"]).add(1);

    let snapshot = recorder.metrics();
    assert_eq!(
        value(&snapshot, "test.plain", &[]),
        Some(SeriesValue::Sum(5.0))
    );
    assert_eq!(
        value(
            &snapshot,
            "log.queue.dropped",
            &[("error.type", "queue_full")]
        ),
        Some(SeriesValue::Sum(4.0))
    );
    let series = snapshot
        .find("log.queue.dropped", &[("error.type", "unwritten")])
        .expect("the series exists");
    assert_eq!(series.value(), &SeriesValue::Sum(1.0));
    assert_eq!(series.unit(), "{record}");
    assert_eq!(series.labels(), [("error.type", "unwritten")]);
}

/// Before a meter is installed a recording reaches no instrument and a completion reads
/// no clock; the instruments still build from the meter installed later.
#[test]
fn a_recording_before_any_meter_records_nothing() {
    assert!(!meter_installed());
    PLAIN.add(1);
    QUEUE.value(3).record();
    WAIT.record(Duration::from_millis(1));
    let guard = completion("test.unrecorded");
    assert!(!guard.records(), "no clock is read without a meter");
    drop(guard);

    let recorder = recorder();
    assert!(recorder.metrics().series().is_empty());
    PLAIN.add(1);
    assert_eq!(
        value(&recorder.metrics(), "test.plain", &[]),
        Some(SeriesValue::Sum(1.0))
    );
}

/// The meter is the process's: a thread that installed no recorder records into it.
#[test]
fn a_thread_without_a_recorder_records_into_the_process_meter() {
    let recorder = recorder();
    std::thread::spawn(|| PLAIN.add(10))
        .join()
        .expect("the other thread does not panic");
    assert_eq!(
        value(&recorder.metrics(), "test.plain", &[]),
        Some(SeriesValue::Sum(10.0))
    );
}

#[test]
fn a_selection_records_only_when_record_runs() {
    let recorder = recorder();
    let selected = QUEUE.value(7);
    assert!(
        recorder.metrics().series().is_empty(),
        "selecting records nothing"
    );
    selected.record();
    assert_eq!(
        value(&recorder.metrics(), "test.queue.length", &[]),
        Some(SeriesValue::Last(7.0))
    );
}

#[test]
fn gauge_keeps_the_latest_value_scaled_into_its_unit() {
    let recorder = recorder();
    QUEUE.value(3).record();
    QUEUE.value(9).record();
    RATIO.value(250.0).record();

    let snapshot = recorder.metrics();
    assert_eq!(
        value(&snapshot, "test.queue.length", &[]),
        Some(SeriesValue::Last(9.0))
    );
    assert_eq!(
        value(&snapshot, "test.ratio", &[]),
        Some(SeriesValue::Last(2.5)),
        "a percent past 100 records a utilization past 1"
    );
}

#[test]
fn a_float_that_measures_nothing_records_nothing() {
    let recorder = recorder();
    for missing in [f64::NAN, f64::INFINITY, -1.0] {
        RATIO.value(missing).record();
    }
    assert_eq!(value(&recorder.metrics(), "test.ratio", &[]), None);

    RATIO.value(50.0).record();
    RATIO.value(f64::NAN).record();
    assert_eq!(
        value(&recorder.metrics(), "test.ratio", &[]),
        Some(SeriesValue::Last(0.5)),
        "a missing value leaves the last one in place, never zero"
    );
}

#[test]
fn the_process_instruments_carry_their_units() {
    let recorder = recorder();
    metrics().memory.value(4096).record();
    metrics().cpu.value(12.5).record();
    let snapshot = recorder.metrics();
    let memory = snapshot
        .find("process.memory.usage", &[])
        .expect("memory is recorded");
    assert_eq!(memory.value(), &SeriesValue::Last(4096.0));
    assert_eq!(memory.unit(), "By");
    let cpu = snapshot
        .find("process.cpu.utilization", &[])
        .expect("CPU is recorded");
    assert_eq!(cpu.value(), &SeriesValue::Last(0.125));
    assert_eq!(cpu.unit(), "1");
}

#[test]
fn histogram_counts_each_value_into_the_bucket_under_its_boundary() {
    let recorder = recorder();
    for millis in [50, 100, 101, 1_000, 5_000] {
        SHORT.record(Duration::from_millis(millis));
    }
    let Some(SeriesValue::Buckets { count, sum, counts }) =
        value(&recorder.metrics(), "test.short.duration", &[])
    else {
        panic!("a histogram holds buckets");
    };
    assert_eq!(count, 5);
    assert!((sum - 6.251).abs() < 1e-9, "sum={sum}");
    assert_eq!(
        counts,
        vec![(Some(0.1), 2), (Some(1.0), 2), (None, 1)],
        "a value on a boundary counts under it"
    );
}

#[test]
fn duration_histogram_defaults_to_the_duration_boundaries_in_seconds() {
    let recorder = recorder();
    WAIT.record(Duration::from_millis(1));
    let snapshot = recorder.metrics();
    let series = snapshot
        .find("test.wait.duration", &[])
        .expect("the wait is recorded");
    assert_eq!(series.unit(), "s");
    let SeriesValue::Buckets { counts, .. } = series.value() else {
        panic!("a histogram holds buckets");
    };
    let boundaries: Vec<f64> = counts.iter().filter_map(|(bound, _)| *bound).collect();
    assert_eq!(boundaries, DURATION_BOUNDARIES_SECONDS);
}

#[test]
fn a_count_histogram_records_counts_in_its_unit() {
    let recorder = recorder();
    for statements in [1, 3, 30] {
        STATEMENTS.labeled(["index"]).record(statements);
    }
    let snapshot = recorder.metrics();
    let series = snapshot
        .find("test.statement.count", &[("db.namespace", "index")])
        .expect("the counts are recorded");
    assert_eq!(series.unit(), "{statement}");
    assert_eq!(
        series.value(),
        &SeriesValue::Buckets {
            count: 3,
            sum: 34.0,
            counts: vec![(Some(1.0), 1), (Some(10.0), 1), (None, 1)],
        }
    );
}

fn traced_question_mark(fail: bool) -> Result<u8, &'static str> {
    let parsed = crate::traced!("test.parse", {
        if fail {
            Err("refused")?;
        }
        1
    });
    Ok(parsed)
}

#[test]
fn a_block_records_one_finished_call_and_its_duration_on_every_path_out() {
    let recorder = recorder();
    let value = crate::traced!("test.block", 2 + 2);
    assert_eq!(value, 4);
    assert_eq!(traced_question_mark(false), Ok(1));
    assert_eq!(traced_question_mark(true), Err("refused"));

    let snapshot = recorder.metrics();
    assert_eq!(calls(&snapshot, "test.block", &OK), Some(1.0));
    assert_eq!(
        calls(&snapshot, "test.parse", &OK),
        Some(2.0),
        "a block left through `?` finished"
    );
    let mut labels = vec![("span.name", "test.block")];
    labels.extend_from_slice(&OK);
    assert!(matches!(
        self::value(&snapshot, "traces.span.metrics.duration", &labels),
        Some(SeriesValue::Buckets { count: 1, .. })
    ));
}

#[test]
fn a_panicking_block_records_an_error_of_type_panic() {
    let recorder = recorder();
    let unwound = std::panic::catch_unwind(|| {
        crate::traced!("test.panics", {
            panic!("the work panics");
        })
    });
    assert!(unwound.is_err());
    assert_eq!(
        calls(&recorder.metrics(), "test.panics", &PANICKED),
        Some(1.0)
    );
}

#[test]
fn a_returned_future_records_a_finished_call() {
    let recorder = recorder();
    let mut work = pin!(crate::traced!("test.future", async { 7 }));
    assert_eq!(poll_once(work.as_mut()), Poll::Ready(7));
    assert_eq!(calls(&recorder.metrics(), "test.future", &OK), Some(1.0));
}

#[test]
fn a_future_dropped_after_its_first_poll_records_a_cancelled_call() {
    let recorder = recorder();
    {
        let mut work = pin!(crate::traced!("test.cancelled", async {
            std::future::pending::<()>().await;
        }));
        assert_eq!(poll_once(work.as_mut()), Poll::Pending);
    }
    let snapshot = recorder.metrics();
    assert_eq!(calls(&snapshot, "test.cancelled", &CANCELLED), Some(1.0));
    assert_eq!(calls(&snapshot, "test.cancelled", &OK), None);
}

#[test]
fn a_future_dropped_before_its_first_poll_records_nothing() {
    let recorder = recorder();
    drop(crate::traced!("test.never.polled", async { 1 }));
    assert!(recorder.metrics().series().is_empty());
}

#[test]
fn a_retained_span_clone_does_not_hold_the_completion_open() {
    let recorder = recorder();
    let retained = crate::traced!("test.retained", crate::Span::current());
    assert_eq!(
        calls(&recorder.metrics(), "test.retained", &OK),
        Some(1.0),
        "the call is recorded when the block ends, while its span is still held"
    );
    drop(retained);
}

#[test]
fn an_operation_whose_span_the_filter_refuses_still_records_its_metrics() {
    let (recorder, _drain) = ScopedRecorder::builder()
        .capture("off")
        .install()
        .expect("`off` parses");
    crate::traced!("test.unsampled", {});
    assert_eq!(calls(&recorder.metrics(), "test.unsampled", &OK), Some(1.0));
}

#[test]
fn every_operation_instrument_names_the_span_metrics_connector_spelling() {
    let recorder = recorder();
    crate::traced!("test.spelled", {});
    let snapshot = recorder.metrics();
    let labels = [("span.name", "test.spelled"), ("status.code", "Ok")];
    let duration = snapshot
        .find("traces.span.metrics.duration", &labels)
        .expect("the duration is recorded");
    let calls = snapshot
        .find("traces.span.metrics.calls", &labels)
        .expect("the call is recorded");
    assert_eq!(duration.unit(), "s");
    assert_eq!(calls.unit(), "{call}");
}

/// The cost of one counter add and one histogram record from one thread, 1,000,000 of
/// each, the median nanoseconds per record over five runs. With `RIFT_COST_METER=1` the
/// process installs a meter first; without it no meter exists and every record is a
/// no-op. Run it alone in a release build, once each way:
///
/// ```text
/// cargo test -p rift-tracing --release --lib -- --ignored --nocapture record_path_cost
/// RIFT_COST_METER=1 cargo test -p rift-tracing --release --lib -- --ignored --nocapture record_path_cost
/// ```
#[test]
#[ignore = "a measurement, not a check; run it alone in a release build"]
fn record_path_cost() {
    use std::time::Instant;

    const RECORDS: u32 = 1_000_000;
    const RUNS: u32 = 5;
    static CALLS: Counter<3> = Counter::declare(
        "test.cost.calls",
        "{call}",
        &["span.name", "status.code", "error.type"],
    );
    static DURATION: Histogram<3> = Histogram::declare(
        "test.cost.duration",
        &["db.namespace", "db.operation.name", "error.type"],
    );

    let recorder = std::env::var_os("RIFT_COST_METER").map(|_| recorder());
    let median = |mut samples: Vec<f64>| {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    };
    let mut counter = Vec::new();
    let mut histogram = Vec::new();
    for _ in 0..RUNS {
        let counter_started = Instant::now();
        for _ in 0..RECORDS {
            CALLS.labeled(["lexical.commit", "Ok", ""]).add(1);
        }
        counter.push(counter_started.elapsed().as_secs_f64() * 1e9 / f64::from(RECORDS));
        let histogram_started = Instant::now();
        for _ in 0..RECORDS {
            DURATION
                .labeled(["index", "exec", ""])
                .record(Duration::from_micros(40));
        }
        histogram.push(histogram_started.elapsed().as_secs_f64() * 1e9 / f64::from(RECORDS));
    }
    println!(
        "meter={} counter_ns_per_record={:.1} histogram_ns_per_record={:.1}",
        meter_installed(),
        median(counter),
        median(histogram)
    );
    if let Some(recorder) = recorder {
        let calls = value(
            &recorder.metrics(),
            "test.cost.calls",
            &[("span.name", "lexical.commit"), ("status.code", "Ok")],
        );
        let expected = f64::from(RECORDS) * f64::from(RUNS);
        assert_eq!(calls, Some(SeriesValue::Sum(expected)), "every add landed");
    }
}
