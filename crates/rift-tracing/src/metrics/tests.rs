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

/// One thread recording into two dispatchers' values in turn lands each value in the
/// values it was recorded into: the thread's cache never answers a point of the other.
#[test]
fn one_thread_recording_into_two_values_in_turn_keeps_them_apart() {
    use super::{MetricValues, labels};

    let first = MetricValues::default();
    let second = MetricValues::default();
    for (values, value) in [(&first, 1.0), (&second, 2.0), (&first, 4.0)] {
        values.add(PLAIN.instrument(), labels([]), value);
        values.set(QUEUE.instrument(), labels([]), value);
        values.observe(SHORT.instrument(), labels([]), value);
    }
    drop(first);
    let third = MetricValues::default();
    third.add(PLAIN.instrument(), labels([]), 8.0);

    let unlabeled = |values: &MetricValues, name: &str| value(&values.snapshot(), name, &[]);
    assert_eq!(
        unlabeled(&second, "test.plain"),
        Some(SeriesValue::Sum(2.0))
    );
    assert_eq!(
        unlabeled(&second, "test.queue.length"),
        Some(SeriesValue::Last(2.0))
    );
    assert!(matches!(
        unlabeled(&second, "test.short.duration"),
        Some(SeriesValue::Buckets { count: 1, .. })
    ));
    assert_eq!(unlabeled(&third, "test.plain"), Some(SeriesValue::Sum(8.0)));
    assert_eq!(unlabeled(&third, "test.queue.length"), None);
}

/// A thread that records into more series than its cache keeps starts its cache over, and
/// every value still lands in its own series.
#[test]
fn a_thread_past_its_cache_bound_still_records_every_value() {
    use super::values::{CACHED_POINTS_MAX, cached_points};
    use super::{MetricValues, labels};

    const WIDE: Counter<1> =
        Counter::declare("test.wide", "{event}", &["test.key"]).series_max(CACHED_POINTS_MAX + 2);
    let keys: Vec<&'static str> = (0..=CACHED_POINTS_MAX)
        .map(|index| &*Box::leak(index.to_string().into_boxed_str()))
        .collect();
    let values = MetricValues::default();
    for round in 0..2 {
        for key in &keys {
            values.add(WIDE.instrument(), labels([*key]), 1.0);
            assert!(cached_points() <= CACHED_POINTS_MAX, "round {round}");
        }
    }

    let snapshot = values.snapshot();
    let wide: Vec<_> = snapshot
        .series()
        .iter()
        .filter(|series| series.name() == "test.wide")
        .collect();
    assert_eq!(wide.len(), keys.len(), "one series per key, no overflow");
    assert!(
        wide.iter()
            .all(|series| series.value() == &SeriesValue::Sum(2.0)),
        "every series holds both rounds"
    );
}

/// Threads recording into one series together leave the exact totals: every shard is
/// added up, and a histogram's count, sum, and buckets agree.
#[test]
fn threads_recording_into_one_series_leave_its_exact_totals() {
    use std::sync::Arc;

    use super::{MetricValues, labels};

    const THREADS: u32 = 16;
    const RECORDS: u32 = 1_000;
    let values = Arc::new(MetricValues::default());
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let values = Arc::clone(&values);
            std::thread::spawn(move || {
                for _ in 0..RECORDS {
                    values.add(PLAIN.instrument(), labels([]), 1.0);
                    values.observe(SHORT.instrument(), labels([]), 0.5);
                    values.observe(SHORT.instrument(), labels([]), 2.0);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("a worker finishes");
    }

    let snapshot = values.snapshot();
    let total = f64::from(THREADS * RECORDS);
    assert_eq!(
        value(&snapshot, "test.plain", &[]),
        Some(SeriesValue::Sum(total))
    );
    let per_bucket = u64::from(THREADS * RECORDS);
    assert_eq!(
        value(&snapshot, "test.short.duration", &[]),
        Some(SeriesValue::Buckets {
            count: 2 * per_bucket,
            sum: total * 2.5,
            counts: vec![(Some(0.1), 0), (Some(1.0), per_bucket), (None, per_bucket)],
        })
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

/// The cost of one record on the handle path, from 1, 4, and 8 threads sharing one
/// dispatcher's values: 1,000,000 counter adds and 1,000,000 histogram records per run,
/// split across the threads, each thread under the same dispatcher a runtime installs.
/// Prints the median nanoseconds per record over five runs of every thread. Run it alone
/// in a release build:
///
/// ```text
/// cargo test -p rift-tracing --release --lib -- --ignored --nocapture record_path_cost
/// ```
#[test]
#[ignore = "a measurement, not a check; run it alone in a release build"]
fn record_path_cost() {
    use std::sync::{Arc, Barrier};
    use std::time::Instant;

    use tracing_subscriber::layer::SubscriberExt;

    use super::{MetricLayer, MetricValues};

    const RECORDS: u32 = 1_000_000;
    const RUNS: u32 = 5;
    const CALLS: Counter<3> = Counter::declare(
        "test.cost.calls",
        "{call}",
        &["span.name", "status.code", "error.type"],
    );
    const DURATION: Histogram<3> = Histogram::declare(
        "test.cost.duration",
        &["db.namespace", "db.operation.name", "error.type"],
    );

    let values = Arc::new(MetricValues::default());
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::registry().with(MetricLayer::new(Arc::clone(&values))),
    );
    let median = |mut samples: Vec<f64>| {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    };
    for threads in [1_u32, 4, 8] {
        let per_thread = RECORDS / threads;
        let mut counter = Vec::new();
        let mut histogram = Vec::new();
        for _ in 0..RUNS {
            let start = Arc::new(Barrier::new(threads as usize));
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    let dispatch = dispatch.clone();
                    let start = Arc::clone(&start);
                    std::thread::spawn(move || {
                        tracing::dispatcher::with_default(&dispatch, || {
                            start.wait();
                            let added = Instant::now();
                            for _ in 0..per_thread {
                                CALLS.labeled(["lexical.commit", "Ok", ""]).add(1);
                            }
                            let added = added.elapsed();
                            let recorded = Instant::now();
                            for _ in 0..per_thread {
                                DURATION
                                    .labeled(["index", "exec", ""])
                                    .record(Duration::from_micros(40));
                            }
                            (added, recorded.elapsed())
                        })
                    })
                })
                .collect();
            for worker in workers {
                let (added, recorded) = worker.join().expect("a worker finishes");
                counter.push(added.as_secs_f64() * 1e9 / f64::from(per_thread));
                histogram.push(recorded.as_secs_f64() * 1e9 / f64::from(per_thread));
            }
        }
        let (counter, histogram) = (median(counter), median(histogram));
        println!(
            "threads={threads} counter_ns_per_record={counter:.1} \
             histogram_ns_per_record={histogram:.1}"
        );
    }
    let snapshot = values.snapshot();
    let calls = snapshot
        .find(
            "test.cost.calls",
            &[("span.name", "lexical.commit"), ("status.code", "Ok")],
        )
        .map(crate::MetricSeries::value);
    let expected = f64::from(RECORDS) * f64::from(RUNS) * 3.0;
    assert!(
        matches!(calls, Some(SeriesValue::Sum(sum)) if (*sum - expected).abs() < 1.0),
        "every add landed: {calls:?}"
    );
}
