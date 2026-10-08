use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use super::{
    CARDINALITY_LIMIT, Counter, DURATION_BOUNDARIES_SECONDS, Gauge, Histogram, InstrumentScope,
    ObservableUpDownCounter, SCOPE, SCOPES_MAX, completion, meter_installed, scopes_built,
};
use crate::{MetricSnapshot, ScopedRecorder, SeriesValue};

static DROPS: Counter<1> = Counter::declare(SCOPE, "test.dropped", "{record}", &["error.type"]);
static PLAIN: Counter<0> = Counter::declare(SCOPE, "test.plain", "{event}", &[]);
static QUEUE: Gauge<u64, 0> = Gauge::declare(SCOPE, "test.queue.length", "{task}", &[]);
static RATIO: Gauge<f64, 0> = Gauge::declare(SCOPE, "test.ratio", "1", &[]);
static HELD: ObservableUpDownCounter<1> =
    ObservableUpDownCounter::declare(SCOPE, "test.held", "{item}", &["test.kind"]);
static CHURNED: ObservableUpDownCounter<0> =
    ObservableUpDownCounter::declare(SCOPE, "test.churned", "{item}", &[]);
static BOUNDED: ObservableUpDownCounter<0> =
    ObservableUpDownCounter::declare(SCOPE, "test.bounded", "{item}", &[]);
static WAIT: Histogram<0> = Histogram::declare(SCOPE, "test.wait.duration", &[]);
static SHORT: Histogram<0> =
    Histogram::declare(SCOPE, "test.short.duration", &[]).boundaries(&[0.1, 1.0]);
static STATEMENTS: Histogram<1, u64> = Histogram::declare_count(
    SCOPE,
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
    let mut labels = vec![("span.name", operation), ("span.kind", "Internal")];
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
        value(&snapshot, "test.dropped", &[("error.type", "queue_full")]),
        Some(SeriesValue::Sum(4.0))
    );
    let series = snapshot
        .find("test.dropped", &[("error.type", "unwritten")])
        .expect("the series exists");
    assert_eq!(series.value(), &SeriesValue::Sum(1.0));
    assert_eq!(series.unit(), "{record}");
    assert_eq!(series.labels(), [("error.type", "unwritten")]);
}

/// Before a meter is installed a recording reaches no instrument and a completion records
/// nothing; the instruments still build from the meter installed later.
#[test]
fn a_recording_before_any_meter_records_nothing() {
    assert!(!meter_installed());
    PLAIN.add(1);
    QUEUE.value(3).record();
    WAIT.record(Duration::from_millis(1));
    let guard = completion(SCOPE, "test.unrecorded");
    assert!(!guard.records(), "no metric is recorded without a meter");
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
fn gauge_keeps_the_latest_value() {
    let recorder = recorder();
    QUEUE.value(3).record();
    QUEUE.value(9).record();
    RATIO.value(2.5).record();

    let snapshot = recorder.metrics();
    assert_eq!(
        value(&snapshot, "test.queue.length", &[]),
        Some(SeriesValue::Last(9.0))
    );
    assert_eq!(
        value(&snapshot, "test.ratio", &[]),
        Some(SeriesValue::Last(2.5))
    );
}

#[test]
fn a_float_that_measures_nothing_records_nothing() {
    let recorder = recorder();
    for missing in [f64::NAN, f64::INFINITY, -1.0] {
        RATIO.value(missing).record();
    }
    assert_eq!(value(&recorder.metrics(), "test.ratio", &[]), None);

    RATIO.value(0.5).record();
    RATIO.value(f64::NAN).record();
    assert_eq!(
        value(&recorder.metrics(), "test.ratio", &[]),
        Some(SeriesValue::Last(0.5)),
        "a missing value leaves the last one in place, never zero"
    );
}

/// Without a meter a read would report nothing, and none registers.
#[test]
fn a_process_without_a_meter_registers_no_observation() {
    assert!(
        HELD.observe(|observation| observation.observe(["any"], 1))
            .is_none()
    );
}

/// A registered read reports at every collection, under its own labels and unit, until its
/// guard drops; the next collection then exports none of its series.
#[test]
fn an_observation_reports_at_each_collection_until_its_guard_drops() {
    let recorder = recorder();
    let held = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(3));
    let read = std::sync::Arc::downgrade(&held);
    let guard = HELD
        .observe(move |observation| {
            if let Some(held) = read.upgrade() {
                observation.observe(["parse"], held.load(std::sync::atomic::Ordering::Relaxed));
            }
        })
        .expect("a recorder installed the meter");
    let held_now = |recorder: &ScopedRecorder| {
        recorder
            .metrics()
            .find("test.held", &[("test.kind", "parse")])
            .map(|series| (series.unit().to_owned(), series.value().clone()))
    };
    assert_eq!(
        held_now(&recorder),
        Some(("{item}".to_owned(), SeriesValue::Sum(3.0)))
    );
    held.store(7, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        held_now(&recorder),
        Some(("{item}".to_owned(), SeriesValue::Sum(7.0))),
        "each collection reads the quantity anew"
    );
    drop(guard);
    assert_eq!(held_now(&recorder), None, "a dropped guard reports nothing");
    assert_eq!(
        std::sync::Arc::strong_count(&held),
        1,
        "the read kept no strong reference to its owner"
    );
}

/// Owners that open and drop a thousand times leave one SDK callback and an empty list:
/// the guard's drop removes its read, and no registration repeats.
#[test]
fn a_thousand_owners_leave_one_callback_and_no_read() {
    let recorder = recorder();
    let before = CHURNED.registered();
    for _ in 0..1_000 {
        let guard = CHURNED
            .observe(|observation| observation.observe([], 1))
            .expect("a recorder installed the meter");
        assert_eq!(CHURNED.registered(), before + 1);
        drop(guard);
    }
    assert!(CHURNED.callback_registered());
    assert_eq!(CHURNED.registered(), before);
    assert_eq!(
        value(&recorder.metrics(), "test.churned", &[]),
        None,
        "no dropped owner reports"
    );
}

/// Past `OBSERVATIONS_MAX` reads the instrument refuses the next one, records the refusal
/// once however many follow, and accepts again once a guard drops.
#[test]
fn a_read_past_the_bound_is_refused_and_recorded_once() -> Result<(), Box<dyn std::error::Error>> {
    let (_recorder, mut drain) = ScopedRecorder::builder().install()?;
    let mut guards: Vec<_> = (0..super::OBSERVATIONS_MAX)
        .map(|_| BOUNDED.observe(|observation| observation.observe([], 1)))
        .collect::<Option<_>>()
        .ok_or("every read under the bound registers")?;
    for _ in 0..3 {
        assert!(BOUNDED.observe(|_| {}).is_none(), "the bound refuses");
    }
    let refusals = drain
        .queued_records()
        .into_iter()
        .filter(|record| record.message() == "observable instrument refused a read past its bound")
        .collect::<Vec<_>>();
    assert_eq!(refusals.len(), 1, "the refusal is recorded once");
    assert_eq!(refusals[0].level(), "warn");
    assert!(refusals[0].fields().contains("test.bounded"));
    guards.pop();
    assert!(
        BOUNDED.observe(|_| {}).is_some(),
        "a dropped guard frees a place"
    );
    Ok(())
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
    let mut labels = vec![("span.name", "test.block"), ("span.kind", "Internal")];
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

/// A failure of the metrics database, a `RiftError` with a registered identity.
fn refused() -> rift_error::RiftError {
    crate::store::store_failure("open", std::path::Path::new("metrics"), "refused")
}

#[test]
fn inline_registered_exits_keep_their_identity_and_control_flow()
-> Result<(), Box<dyn std::error::Error>> {
    fn question() -> Result<u8, rift_error::RiftError> {
        crate::traced!("test.question_error", {
            let _child =
                tracing::info_span!("test.question_child", error.type = "timeout").entered();
            Err::<(), _>(refused())?;
        });
        Ok(9)
    }
    fn returned() -> Result<u8, rift_error::RiftError> {
        crate::traced!("test.return_error", {
            let _child = tracing::info_span!("test.return_child", error.type = "timeout").entered();
            return Err(SourceError(refused()).into());
        });
    }
    struct SourceError(rift_error::RiftError);
    impl From<SourceError> for rift_error::RiftError {
        fn from(source: SourceError) -> Self {
            assert_eq!(
                source.0.slug(),
                rift_error::errors::tracing::log_store_failed::SLUG
            );
            let failed: Result<(), rift_error::RiftError> =
                rift_error::errors::tracing::log_batch_limit()
                    .observed(2_u64)
                    .maximum(1_u64)
                    .fail();
            failed.expect_err("the builder returns its registered error")
        }
    }
    fn converted() -> Result<u8, rift_error::RiftError> {
        crate::traced!("test.converted_error", {
            let _child =
                tracing::info_span!("test.converted_child", error.type = "timeout").entered();
            let __rift_entered = tracing::Span::none();
            assert!(__rift_entered.is_disabled());
            Err::<(), _>(SourceError(refused()))?;
        });
        Ok(9)
    }
    let (recorder, mut drain) = crate::ScopedRecorder::builder().install()?;
    assert!(question().is_err());
    assert!(returned().is_err());
    assert_eq!(
        converted().expect_err("conversion fails").slug(),
        rift_error::errors::tracing::log_batch_limit::SLUG
    );
    let snapshot = recorder.metrics();
    drop(recorder);
    let identity = rift_error::errors::tracing::log_store_failed::SLUG.as_str();
    let records = drain.queued_records();
    for (operation, identity) in [
        ("test.question_error", identity),
        (
            "test.return_error",
            rift_error::errors::tracing::log_batch_limit::SLUG.as_str(),
        ),
        (
            "test.converted_error",
            rift_error::errors::tracing::log_batch_limit::SLUG.as_str(),
        ),
    ] {
        assert_eq!(
            calls(
                &snapshot,
                operation,
                &[("status.code", "Error"), ("error.type", identity)]
            ),
            Some(1.0)
        );
        let record = records
            .iter()
            .find(|record| record.message() == operation)
            .ok_or("the operation closes")?;
        let fields: serde_json::Value = serde_json::from_str(record.fields())?;
        assert_eq!(fields["error.type"], identity);
        assert_eq!(fields["status.code"], "Error");
    }
    for child in [
        "test.question_child",
        "test.return_child",
        "test.converted_child",
    ] {
        let record = records
            .iter()
            .find(|record| record.message() == child)
            .ok_or("the child closes")?;
        let fields: serde_json::Value = serde_json::from_str(record.fields())?;
        assert_eq!(fields["error.type"], "timeout");
    }
    Ok(())
}

#[test]
fn inline_polled_exits_keep_their_registered_identity() -> Result<(), Box<dyn std::error::Error>> {
    fn polled() -> Poll<Result<u8, rift_error::RiftError>> {
        crate::traced!("test.poll_error", {
            let _ = Poll::Ready(Err::<(), _>(refused()))?;
        });
        Poll::Ready(Ok(9))
    }
    fn polled_option() -> Poll<Option<Result<u8, rift_error::RiftError>>> {
        crate::traced!("test.poll_option_error", {
            let _ = Poll::Ready(Some(Err::<(), _>(refused())))?;
        });
        Poll::Ready(Some(Ok(9)))
    }
    let (recorder, mut drain) = crate::ScopedRecorder::builder().install()?;
    assert!(matches!(polled(), Poll::Ready(Err(_))));
    assert!(matches!(polled_option(), Poll::Ready(Some(Err(_)))));
    let snapshot = recorder.metrics();
    drop(recorder);
    let records = drain.queued_records();
    let identity = rift_error::errors::tracing::log_store_failed::SLUG.as_str();
    for operation in ["test.poll_error", "test.poll_option_error"] {
        assert_eq!(
            calls(
                &snapshot,
                operation,
                &[("status.code", "Error"), ("error.type", identity)]
            ),
            Some(1.0)
        );
        let record = records
            .iter()
            .find(|record| record.message() == operation)
            .ok_or("the operation closes")?;
        let fields: serde_json::Value = serde_json::from_str(record.fields())?;
        assert_eq!(fields["error.type"], identity);
        assert_eq!(fields["status.code"], "Error");
    }
    Ok(())
}

#[test]
fn inline_exits_keep_nested_functions_and_option_control_flow() {
    fn option() -> Option<u8> {
        crate::traced!("test.option_exit", {
            None::<u8>?;
        });
        Some(9)
    }
    fn control() -> std::ops::ControlFlow<u8, ()> {
        crate::traced!("test.control_exit", {
            std::ops::ControlFlow::<u8, ()>::Break(4)?;
        });
        std::ops::ControlFlow::Continue(())
    }
    let recorder = recorder();
    assert_eq!(option(), None);
    assert_eq!(control(), std::ops::ControlFlow::Break(4));
    let value = crate::traced!("test.nested_exits", {
        fn nested(early: bool) -> Result<(), rift_error::RiftError> {
            if early {
                return Err(refused());
            }
            Ok(())
        }
        let closure = || -> Result<(), rift_error::RiftError> {
            Err::<(), _>(refused())?;
            Ok(())
        };
        assert!(nested(true).is_err());
        assert!(closure().is_err());
        7
    });
    assert_eq!(value, 7);
    for operation in ["test.option_exit", "test.control_exit", "test.nested_exits"] {
        assert_eq!(calls(&recorder.metrics(), operation, &OK), Some(1.0));
    }
}

/// A block whose value is `Err(RiftError)` records the error's registered identity as its
/// `error.type` label; `Ok`, an error of another type, and a value whose type the caller's
/// annotation infers record a finished call.
#[test]
fn a_block_returning_a_registered_error_records_its_identity() {
    let recorder = recorder();
    let failed: Result<u8, rift_error::RiftError> =
        crate::traced!("test.returns_error", { Err(refused()) });
    let finished: Result<u8, rift_error::RiftError> = crate::traced!("test.returns_ok", { Ok(1) });
    let other: Result<u8, &str> = crate::traced!("test.returns_other", { Err("refused") });
    let parsed: Result<u8, std::num::ParseIntError> = crate::traced!("test.parses", "7".parse());
    assert!(failed.is_err());
    assert_eq!(
        (finished.ok(), other, parsed),
        (Some(1), Err("refused"), Ok(7))
    );

    let identity = rift_error::errors::tracing::log_store_failed::SLUG.as_str();
    let snapshot = recorder.metrics();
    assert_eq!(
        calls(
            &snapshot,
            "test.returns_error",
            &[("status.code", "Error"), ("error.type", identity)]
        ),
        Some(1.0),
        "{snapshot:?}"
    );
    for operation in ["test.returns_ok", "test.returns_other", "test.parses"] {
        assert_eq!(calls(&snapshot, operation, &OK), Some(1.0), "{operation}");
    }
}

/// An awaited operation whose output is `Err(RiftError)` records the error's registered
/// identity as its `error.type` label.
#[test]
fn a_future_returning_a_registered_error_records_its_identity() {
    let recorder = recorder();
    let mut work = pin!(crate::traced!("test.awaits_error", async {
        Err::<u8, _>(refused())
    }));
    assert!(matches!(poll_once(work.as_mut()), Poll::Ready(Err(_))));

    let identity = rift_error::errors::tracing::log_store_failed::SLUG.as_str();
    assert_eq!(
        calls(
            &recorder.metrics(),
            "test.awaits_error",
            &[("status.code", "Error"), ("error.type", identity)]
        ),
        Some(1.0)
    );
}

/// Leaves a block through `return`: the block diverges, and its type falls back to `!`.
fn traced_return() -> u8 {
    crate::traced!("test.diverges", {
        return 3;
    })
}

/// A block that diverges compiles, and a block left through `return` records a finished
/// call: the work's value never exists, so no identity is read.
#[test]
fn a_diverging_block_records_a_finished_call() {
    let recorder = recorder();
    assert_eq!(traced_return(), 3);
    assert_eq!(calls(&recorder.metrics(), "test.diverges", &OK), Some(1.0));
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

/// A clock the test moves by hand, and the reading [`ScopedRecorderBuilder::clock`]
/// takes from it: the nanoseconds added to it since its epoch.
///
/// [`ScopedRecorderBuilder::clock`]: crate::ScopedRecorderBuilder::clock
fn manual_clock() -> (
    Arc<AtomicU64>,
    impl Fn() -> Duration + Send + Sync + 'static,
) {
    let nanoseconds = Arc::new(AtomicU64::new(0));
    let reading = Arc::clone(&nanoseconds);
    (nanoseconds, move || {
        Duration::from_nanos(reading.load(Ordering::Relaxed))
    })
}

/// The duration histogram records the elapsed time the operation's close record states as
/// `elapsed_ms`: one reading at the end of the work. A clone of the span held past the
/// work lengthens neither.
#[test]
fn the_duration_histogram_records_the_elapsed_time_of_the_close_record()
-> Result<(), Box<dyn std::error::Error>> {
    let (clock, now) = manual_clock();
    let (recorder, mut drain) = ScopedRecorder::builder().clock(now).install()?;
    let retained = crate::traced!("test.timed", {
        clock.fetch_add(250_000_000, Ordering::Relaxed);
        crate::Span::current()
    });
    clock.fetch_add(4_000_000_000, Ordering::Relaxed);
    drop(retained);
    let snapshot = recorder.metrics();
    drop(recorder);

    let records = drain.queued_records();
    let closed = records
        .iter()
        .find(|record| record.message() == "test.timed")
        .ok_or("the operation's span wrote its close record")?;
    let fields: serde_json::Value = serde_json::from_str(closed.fields())?;
    assert_eq!(fields["elapsed_ms"], "250", "{fields}");
    let mut labels = vec![("span.name", "test.timed"), ("span.kind", "Internal")];
    labels.extend_from_slice(&OK);
    let Some(SeriesValue::Buckets { count, sum, .. }) =
        value(&snapshot, "traces.span.metrics.duration", &labels)
    else {
        return Err(format!("the duration histogram holds the operation: {snapshot:?}").into());
    };
    assert_eq!((count, sum), (1, 0.25), "250 ms, in seconds");
    Ok(())
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
    let labels = [
        ("span.name", "test.spelled"),
        ("span.kind", "Internal"),
        ("status.code", "Ok"),
    ];
    let duration = snapshot
        .find("traces.span.metrics.duration", &labels)
        .expect("the duration is recorded");
    let calls = snapshot
        .find("traces.span.metrics.calls", &labels)
        .expect("the call is recorded");
    assert_eq!(duration.unit(), "s");
    assert_eq!(calls.unit(), "{call}");
    for series in [duration, calls] {
        assert_eq!(
            (series.scope_name(), series.scope_version()),
            ("rift-tracing", env!("CARGO_PKG_VERSION")),
            "a `traced!` operation exports under the scope of the crate it expands in"
        );
    }
    assert_eq!(
        calls.labels(),
        [
            ("span.kind", "Internal"),
            ("span.name", "test.spelled"),
            ("status.code", "Ok"),
        ],
        "the kind is the one the exported span carries"
    );
}

/// A declaration exports under the scope it names: the crate's name and version.
#[test]
fn a_declaration_exports_under_the_scope_it_names() {
    let recorder = recorder();
    let other = Counter::<0>::declare(
        InstrumentScope::new("test-emitter", "1.2.3"),
        "test.scoped",
        "{event}",
        &[],
    );
    other.add(1);
    PLAIN.add(1);
    let snapshot = recorder.metrics();
    let scoped = snapshot
        .find("test.scoped", &[])
        .expect("the scoped counter is exported");
    assert_eq!(
        (scoped.scope_name(), scoped.scope_version()),
        ("test-emitter", "1.2.3")
    );
    let plain = snapshot
        .find("test.plain", &[])
        .expect("the plain counter is exported");
    assert_eq!(
        (plain.scope_name(), plain.scope_version()),
        (SCOPE.name(), SCOPE.version())
    );
    assert!(
        scoped
            .to_string()
            .contains("otel.scope.name=test-emitter otel.scope.version=1.2.3"),
        "the printed line names the scope: {scoped}"
    );
}

/// Past `SCOPES_MAX` scopes the process builds no meter for the next one: its instruments
/// record nothing, the refusal is recorded once however many follow, and the scopes built
/// before keep recording.
#[test]
fn a_scope_past_the_bound_is_refused_and_recorded_once() -> Result<(), Box<dyn std::error::Error>> {
    const NAMES: [&str; SCOPES_MAX + 1] = [
        "test-scope-0",
        "test-scope-1",
        "test-scope-2",
        "test-scope-3",
        "test-scope-4",
        "test-scope-5",
        "test-scope-6",
        "test-scope-7",
    ];
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    assert_eq!(
        scopes_built(),
        1,
        "the install builds `rift-tracing`'s scope"
    );
    let counters = NAMES.map(|name| {
        Counter::<0>::declare(
            InstrumentScope::new(name, "0.0.0"),
            "test.bounded.scope",
            "{event}",
            &[],
        )
    });
    for counter in &counters {
        counter.add(1);
    }
    counters[SCOPES_MAX].add(1);
    assert_eq!(scopes_built(), SCOPES_MAX, "the table holds the bound");
    let snapshot = recorder.metrics();
    let exported: Vec<&str> = snapshot
        .series()
        .iter()
        .filter(|series| series.name() == "test.bounded.scope")
        .map(crate::MetricSeries::scope_name)
        .collect();
    assert_eq!(
        exported,
        NAMES[..SCOPES_MAX - 1],
        "every scope under the bound exports; the scopes past it record nothing"
    );
    let refusals = drain
        .queued_records()
        .into_iter()
        .filter(|record| {
            record.message() == "meter refused an instrumentation scope past its bound"
        })
        .collect::<Vec<_>>();
    assert_eq!(refusals.len(), 1, "the refusal is recorded once");
    assert_eq!(refusals[0].level(), "warn");
    assert!(refusals[0].fields().contains(NAMES[SCOPES_MAX - 1]));
    PLAIN.add(1);
    assert!(
        recorder.metrics().find("test.plain", &[]).is_some(),
        "a scope built before the bound keeps recording"
    );
    Ok(())
}

/// An instrument aggregates at most [`CARDINALITY_LIMIT`] series; one more label set
/// records into the series the SDK labels `otel.metric.overflow`.
#[test]
fn an_instrument_past_its_cardinality_limit_records_into_the_overflow_series() {
    static WIDE: Counter<1> = Counter::declare(SCOPE, "test.wide", "{event}", &["test.key"]);
    let recorder = recorder();
    for index in 0..=CARDINALITY_LIMIT {
        let value: &'static str = Box::leak(index.to_string().into_boxed_str());
        WIDE.labeled([value]).add(1);
    }

    let snapshot = recorder.metrics();
    let series: Vec<_> = snapshot
        .series()
        .iter()
        .filter(|series| series.name() == "test.wide")
        .collect();
    assert_eq!(
        series.len(),
        CARDINALITY_LIMIT + 1,
        "the bound, then overflow"
    );
    assert_eq!(
        value(&snapshot, "test.wide", &[("otel.metric.overflow", "true")]),
        Some(SeriesValue::Sum(1.0)),
        "the label set past the bound joins the overflow series"
    );
}

/// The view bounds each instrument at the limit it is given, and keeps the instrument's
/// name, unit, and histogram boundaries.
#[test]
fn the_cardinality_view_bounds_each_instrument_at_its_limit() {
    use opentelemetry::KeyValue;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .with_view(super::cardinality_view(2))
        .build();
    let meter = provider.meter_with_scope(SCOPE.instrumentation_scope());
    let counter = meter.u64_counter("test.view").with_unit("{event}").build();
    let histogram = meter
        .f64_histogram("test.view.duration")
        .with_unit("s")
        .with_boundaries(vec![0.5])
        .build();
    for key in ["a", "b", "c"] {
        counter.add(1, &[KeyValue::new("test.key", key)]);
    }
    histogram.record(0.1, &[]);
    provider.force_flush().expect("the provider flushes");

    let finished = exporter.get_finished_metrics().expect("an export");
    let metrics: Vec<_> = finished
        .last()
        .expect("one export")
        .scope_metrics()
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .collect();
    let view = metrics
        .iter()
        .find(|metric| metric.name() == "test.view")
        .expect("the counter exports under its own name");
    assert_eq!(view.unit(), "{event}");
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = view.data() else {
        panic!("a counter exports a sum");
    };
    let overflowed = sum
        .data_points()
        .filter(|point| {
            point
                .attributes()
                .any(|pair| pair.key.as_str() == "otel.metric.overflow")
        })
        .count();
    assert_eq!(sum.data_points().count(), 3, "two series and the overflow");
    assert_eq!(overflowed, 1);
    let duration = metrics
        .iter()
        .find(|metric| metric.name() == "test.view.duration")
        .expect("the histogram exports under its own name");
    let AggregatedMetrics::F64(MetricData::Histogram(buckets)) = duration.data() else {
        panic!("a histogram exports buckets");
    };
    let bounds: Vec<f64> = buckets
        .data_points()
        .flat_map(opentelemetry_sdk::metrics::data::HistogramDataPoint::bounds)
        .collect();
    assert_eq!(bounds, [0.5], "the declared boundaries stay");
}

/// [`Counter::add_built`] records only once [`Counter::build`] built the instrument.
#[test]
fn a_counter_records_through_add_built_only_once_built() {
    static LATE: Counter<0> = Counter::declare(SCOPE, "test.late", "{event}", &[]);
    let recorder = recorder();
    LATE.add_built([], 1);
    assert_eq!(value(&recorder.metrics(), "test.late", &[]), None);
    LATE.build();
    LATE.add_built([], 2);
    assert_eq!(
        value(&recorder.metrics(), "test.late", &[]),
        Some(SeriesValue::Sum(2.0))
    );
}

/// The cost of one counter add and one histogram record from one thread, 1,000,000 of
/// each, over five runs. `RIFT_COST_METER=1` installs a meter before timing; otherwise
/// every timed record is a no-op. The test emits each sample after timing, and the Python
/// collector computes medians from retained logs.
#[test]
#[ignore = "a measurement, not a check; run it alone in a release build"]
fn record_path_cost() {
    use std::time::Instant;

    const RECORDS: u32 = 1_000_000;
    const RUNS: u32 = 5;
    static CALLS: Counter<3> = Counter::declare(
        SCOPE,
        "test.cost.calls",
        "{call}",
        &["span.name", "status.code", "error.type"],
    );
    static DURATION: Histogram<3> = Histogram::declare(
        SCOPE,
        "test.cost.duration",
        &["db.namespace", "db.operation.name", "error.type"],
    );

    let recorder = std::env::var_os("RIFT_COST_METER").map(|_| recorder());
    let mut samples = Vec::with_capacity(RUNS as usize);
    for _ in 0..RUNS {
        let counter_started = Instant::now();
        for _ in 0..RECORDS {
            CALLS.labeled(["lexical.commit", "Ok", ""]).add(1);
        }
        let counter_ns_per_record =
            counter_started.elapsed().as_secs_f64() * 1e9 / f64::from(RECORDS);
        let histogram_started = Instant::now();
        for _ in 0..RECORDS {
            DURATION
                .labeled(["index", "exec", ""])
                .record(Duration::from_micros(40));
        }
        let histogram_ns_per_record =
            histogram_started.elapsed().as_secs_f64() * 1e9 / f64::from(RECORDS);
        samples.push((
            meter_installed(),
            counter_ns_per_record,
            histogram_ns_per_record,
        ));
    }
    for (meter, counter_ns_per_record, histogram_ns_per_record) in samples {
        crate::info!(
            meter,
            counter_ns_per_record,
            histogram_ns_per_record,
            "record_path_cost"
        );
    }
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
