use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use super::{
    Counter, DURATION_BOUNDARIES_SECONDS, Gauge, Histogram, InstrumentKind, MetricSnapshot,
    OPERATION_CALLS, OPERATION_DURATION, SeriesValue, completion, metrics,
};
use crate::ScopedRecorder;
use crate::sampler::{ProcessReading, SampleSeries};

const DROPS: Counter<1> = Counter::declare("log.queue.dropped", "{record}", &["error.type"]);
const PLAIN: Counter<0> = Counter::declare("test.plain", "{event}", &[]);
const BOUNDED: Counter<1> =
    Counter::declare("test.bounded", "{event}", &["test.key"]).series_max(2);
const QUEUE: Gauge<u64, 0> = Gauge::declare("test.queue.length", "{task}", &[]);
const RATIO: Gauge<f64, 0> = Gauge::declare("test.ratio", "1", &[]).scaled(0.01);
const WAIT: Histogram<0> = Histogram::declare("test.wait.duration", &[]);
const SHORT: Histogram<0> = Histogram::declare("test.short.duration", &[]).boundaries(&[0.1, 1.0]);

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
    match value(snapshot, OPERATION_CALLS.instrument().name(), &labels)? {
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
    assert_eq!(
        value(
            &snapshot,
            "log.queue.dropped",
            &[("error.type", "unwritten")]
        ),
        Some(SeriesValue::Sum(1.0))
    );
    let series = snapshot
        .find("log.queue.dropped", &[("error.type", "unwritten")])
        .expect("the series exists");
    assert_eq!(series.instrument().kind(), InstrumentKind::Counter);
    assert_eq!(series.instrument().unit(), "{record}");
    assert_eq!(series.labels(), &[("error.type", "unwritten")]);
}

#[test]
fn a_thread_without_metric_values_records_nothing() {
    PLAIN.add(1);
    QUEUE.value(3).record();
    WAIT.record(Duration::from_millis(1));
    let guard = completion("test.unrecorded");
    assert!(!guard.records(), "no clock is read without metric values");
    drop(guard);

    let recorder = recorder();
    assert!(recorder.metrics().series().is_empty());
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
fn current_reads_the_latest_published_sample_and_records_nothing_without_one() {
    let recorder = recorder();
    metrics().memory.current().record();
    metrics().cpu.current().record();
    assert!(
        recorder.metrics().series().is_empty(),
        "no sample, no value"
    );
    assert_eq!(recorder.metrics().sampled_at_ms(), None);

    let mut series = SampleSeries::default();
    let reading = ProcessReading {
        resident_bytes: Some(64 << 20),
        cpu_percent: Some(150.0),
        ..ProcessReading::default()
    };
    let first = series.observe(reading, Duration::ZERO, 1_000);
    recorder.values().publish_sample(first);
    metrics().memory.current().record();
    metrics().cpu.current().record();
    let snapshot = recorder.metrics();
    assert_eq!(
        value(&snapshot, "process.memory.usage", &[]),
        Some(SeriesValue::Last(f64::from(64_u32 << 20)))
    );
    assert_eq!(
        value(&snapshot, "process.cpu.utilization", &[]),
        None,
        "the first refresh measures no CPU usage"
    );
    assert_eq!(snapshot.sampled_at_ms(), Some(1_000));

    let second = series.observe(reading, Duration::from_secs(1), 2_000);
    recorder.values().publish_sample(second);
    metrics().cpu.current().record();
    assert_eq!(
        value(&recorder.metrics(), "process.cpu.utilization", &[]),
        Some(SeriesValue::Last(1.5))
    );
}

#[test]
fn caller_values_reuse_the_process_instruments() {
    let recorder = recorder();
    metrics().memory.value(4096).record();
    metrics().cpu.value(12.5).record();
    let snapshot = recorder.metrics();
    assert_eq!(
        value(&snapshot, "process.memory.usage", &[]),
        Some(SeriesValue::Last(4096.0))
    );
    assert_eq!(
        value(&snapshot, "process.cpu.utilization", &[]),
        Some(SeriesValue::Last(0.125))
    );
    assert_eq!(metrics().cpu.instrument().unit(), "1");
    assert_eq!(metrics().memory.instrument().unit(), "By");
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
    assert_eq!(WAIT.instrument().unit(), "s");
    assert_eq!(WAIT.instrument().boundaries(), &DURATION_BOUNDARIES_SECONDS);
    assert!(
        DURATION_BOUNDARIES_SECONDS
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "boundaries ascend"
    );
}

#[test]
fn label_sets_past_the_bound_record_into_one_overflow_series() {
    let recorder = recorder();
    for key in ["a", "b", "c", "d", "a"] {
        BOUNDED.labeled([key]).add(1);
    }
    let snapshot = recorder.metrics();
    let bounded: Vec<_> = snapshot
        .series()
        .iter()
        .filter(|series| series.name() == "test.bounded")
        .collect();
    assert_eq!(bounded.len(), 3, "two label sets and the overflow series");
    assert_eq!(
        value(&snapshot, "test.bounded", &[("test.key", "a")]),
        Some(SeriesValue::Sum(2.0))
    );
    let overflow = bounded.last().expect("the overflow series sorts last");
    assert!(overflow.is_overflow());
    assert!(overflow.labels().is_empty());
    assert_eq!(
        overflow.value(),
        &SeriesValue::Sum(2.0),
        "c and d overflowed"
    );
}

#[test]
fn two_threads_record_into_their_own_recorders() {
    let recorder = recorder();
    PLAIN.add(1);
    std::thread::spawn(|| {
        let other = self::recorder();
        PLAIN.add(10);
        assert_eq!(
            value(&other.metrics(), "test.plain", &[]),
            Some(SeriesValue::Sum(10.0))
        );
    })
    .join()
    .expect("the other thread does not panic");
    assert_eq!(
        value(&recorder.metrics(), "test.plain", &[]),
        Some(SeriesValue::Sum(1.0))
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
    let duration = value_of_duration(&snapshot, "test.block", &OK);
    assert_eq!(duration, 1);
}

fn value_of_duration(snapshot: &MetricSnapshot, operation: &str, outcome: &[(&str, &str)]) -> u64 {
    let mut labels = vec![("span.name", operation)];
    labels.extend_from_slice(outcome);
    match value(snapshot, OPERATION_DURATION.instrument().name(), &labels) {
        Some(SeriesValue::Buckets { count, .. }) => count,
        other => panic!("the duration histogram holds buckets, not {other:?}"),
    }
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
    let duration = OPERATION_DURATION.instrument();
    let calls = OPERATION_CALLS.instrument();
    assert_eq!(duration.name(), "traces.span.metrics.duration");
    assert_eq!(duration.unit(), "s");
    assert_eq!(calls.name(), "traces.span.metrics.calls");
    assert_eq!(calls.unit(), "{call}");
    assert_eq!(
        duration.label_keys(),
        &["span.name", "status.code", "error.type"]
    );
    assert_eq!(calls.label_keys(), duration.label_keys());
    assert_eq!(duration.series_max(), calls.series_max());
}
