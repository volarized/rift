use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use opentelemetry::trace::TraceContextExt as _;

use super::{SCOPED_RECORDER_STREAM_VARIABLE, ScopedRecorder};
use crate::SeriesValue;
use crate::record::LogRecord;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The messages `records` carries, oldest first.
fn messages(records: &[LogRecord]) -> Vec<&str> {
    records.iter().map(LogRecord::message).collect()
}

#[test]
fn a_recorder_captures_every_level_on_its_thread_by_default() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    crate::trace!(
        component = "index",
        operation = "index.publish",
        epoch = 3,
        "traced"
    );
    crate::error!("failed");
    drop(recorder);

    let records = drain.queued_records();
    assert_eq!(messages(&records), ["traced", "failed"]);
    assert_eq!(records[0].level(), "trace");
    assert_eq!(records[0].component(), "index");
    assert_eq!(records[0].operation(), "index.publish");
    assert_eq!(
        records[0].fields(),
        "{\"code.function.name\":\"rift_tracing::recorder::tests::\
         a_recorder_captures_every_level_on_its_thread_by_default\",\"epoch\":\"3\"}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scoped_span_close_keeps_its_context_on_another_recorder_thread() -> TestResult {
    if !crate::otlp::test_process_export_configured() {
        return Ok(());
    }

    let (origin_recorder, mut drain) = ScopedRecorder::builder().install()?;
    let span = crate::info_span!("search.request", component = "index");
    let id =
        Option::<tracing::span::Id>::from(&span).expect("the scoped recorder enables the span");
    let context = tracing::dispatcher::get_default(|dispatch| {
        tracing_opentelemetry::get_otel_context(&id, dispatch)
    })
    .expect("the scoped recorder owns the span's OpenTelemetry context");
    let span_context = context.span().span_context().clone();
    assert!(span_context.is_valid());

    span.in_scope(|| {
        crate::info!(
            component = "index",
            expected_trace_id = %span_context.trace_id(),
            expected_span_id = %span_context.span_id(),
            "scoped async close event"
        );
    });

    tokio::spawn(async move {
        let (foreign_recorder, _foreign_drain) = ScopedRecorder::builder()
            .install()
            .expect("the foreign recorder filter parses");
        drop(span);
        drop(foreign_recorder);
    })
    .await
    .expect("the foreign recorder task completes");
    drop(origin_recorder);

    assert!(messages(&drain.queued_records()).contains(&"scoped async close event"));
    Ok(())
}

#[test]
fn the_capture_filter_selects_what_the_recorder_keeps() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder()
        .capture("rift_tracing=warn")
        .install()?;
    crate::info!("below the filter");
    crate::warn!("admitted");
    drop(recorder);

    assert_eq!(messages(&drain.queued_records()), ["admitted"]);
    Ok(())
}

#[test]
fn a_filter_that_does_not_parse_is_refused() {
    let refused = ScopedRecorder::builder().capture("rift=loud").install();

    assert!(refused.is_err(), "an unknown level is not a filter");
}

/// Dropping a recorder restores the default it replaced: the inner recorder takes the
/// records while held, the outer one takes them again after it drops, and none is
/// captured once both are gone.
#[test]
fn nested_recorders_restore_the_previous_default_when_dropped() -> TestResult {
    let (outer, mut outer_drain) = ScopedRecorder::builder().install()?;
    crate::info!("first outer");
    let (inner, mut inner_drain) = ScopedRecorder::builder().install()?;
    crate::info!("inner");
    drop(inner);
    crate::info!("second outer");
    drop(outer);
    crate::info!("after both");

    assert_eq!(messages(&inner_drain.queued_records()), ["inner"]);
    assert_eq!(
        messages(&outer_drain.queued_records()),
        ["first outer", "second outer"]
    );
    Ok(())
}

/// A thread the test spawns runs under its own default: the installing thread's recorder
/// captures nothing from it. The thread installs a recorder of its own and hands its
/// drain back through `join`, which also orders the reads after the emits.
#[test]
fn a_spawned_thread_records_only_into_a_recorder_it_installs() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;

    let spawned = std::thread::spawn(|| {
        crate::info!("before the thread's recorder");
        let (thread_recorder, thread_drain) = ScopedRecorder::builder().install()?;
        crate::info!("inside the thread's recorder");
        drop(thread_recorder);
        Ok::<_, crate::LogFilterError>(thread_drain)
    });
    let mut thread_drain = spawned.join().map_err(|_| "the thread does not panic")??;
    drop(recorder);

    assert!(drain.queued_records().is_empty());
    assert_eq!(
        messages(&thread_drain.queued_records()),
        ["inside the thread's recorder"]
    );
    Ok(())
}

#[test]
fn a_scoped_recorder_snapshot_keeps_traced_metrics_and_records() -> TestResult {
    let clock = Arc::new(AtomicU64::new(0));
    let reading = Arc::clone(&clock);
    let (recorder, mut drain) = ScopedRecorder::builder()
        .clock(move || Duration::from_nanos(reading.load(Ordering::Relaxed)))
        .install()?;
    crate::warn!(component = "index", "recorded before the operation");
    crate::traced!("test.printed", {
        clock.fetch_add(250_000_000, Ordering::Relaxed);
    });

    let snapshot = recorder.metrics();
    drop(recorder);

    assert!(messages(&drain.queued_records()).contains(&"recorded before the operation"));
    assert_eq!(
        snapshot
            .find(
                "traces.span.metrics.calls",
                &[
                    ("span.name", "test.printed"),
                    ("span.kind", "Internal"),
                    ("status.code", "Ok"),
                ],
            )
            .map(|series| series.value().clone()),
        Some(SeriesValue::Sum(1.0))
    );
    match snapshot
        .find(
            "traces.span.metrics.duration",
            &[
                ("span.name", "test.printed"),
                ("span.kind", "Internal"),
                ("status.code", "Ok"),
            ],
        )
        .map(super::metrics::MetricSeries::value)
    {
        Some(SeriesValue::Buckets { count, sum, .. }) => {
            assert_eq!(*count, 1);
            assert!((*sum - 0.25).abs() < f64::EPSILON);
        }
        other => panic!("expected duration histogram, got {other:?}"),
    }
    Ok(())
}

/// The clock a builder names is the one the recorder's thread reads, through
/// `monotonic_now` and `measure_elapsed!`; another thread keeps the process clock.
#[test]
fn a_recorder_clock_is_read_on_its_thread_alone() -> TestResult {
    let far = Duration::from_secs(1 << 40);
    let clock = Arc::new(AtomicU64::new(0));
    let reading = Arc::clone(&clock);
    let (recorder, _drain) = ScopedRecorder::builder()
        .clock(move || far + Duration::from_millis(reading.load(Ordering::Relaxed)))
        .install()?;

    let ((), measurement) = crate::measure_elapsed!("test.measured", {
        clock.fetch_add(40, Ordering::Relaxed);
    })?;
    let here = crate::__private::monotonic_now();
    let elsewhere = std::thread::spawn(crate::__private::monotonic_now)
        .join()
        .map_err(|_| "the thread does not panic")?;
    drop(recorder);

    assert_eq!(measurement.elapsed(), Duration::from_millis(40));
    assert_eq!(here, far + Duration::from_millis(40));
    assert!(elsewhere < far, "{elsewhere:?}");
    assert!(
        crate::__private::monotonic_now() < far,
        "the clock ends with the recorder"
    );
    Ok(())
}

/// Selects what [`unscoped_export_child`] records when a test starts it; unset, it records
/// nothing.
const UNSCOPED_CHILD_VARIABLE: &str = "RIFT_TRACING_UNSCOPED_CHILD";

extern "C" fn assert_unscoped_export_shutdown_succeeded() {
    if !super::UNSCOPED_EXPORT_SHUTDOWN_SUCCEEDED.load(Ordering::Acquire) {
        std::process::abort();
    }
}

/// The child process the unscoped exporter tests start, as nextest would: `--exact` and
/// `--nocapture`. Modes cover sync, Tokio runtimes, unwind, and scoped recorder installs.
#[test]
fn unscoped_export_child() -> TestResult {
    let Some(mode) = std::env::var_os(UNSCOPED_CHILD_VARIABLE) else {
        return Ok(());
    };
    let shutdown_export = !matches!(mode.to_str(), Some("no-shutdown" | "unwind"));
    if !shutdown_export {
        assert!(shutdown_hooks::add_shutdown_hook(
            assert_unscoped_export_shutdown_succeeded
        ));
    }
    if mode == "first-scoped" {
        record_first_scoped_metrics()?;
    } else if mode == "scoped" {
        record_unscoped_signals("scoped.before");
        let (recorder, mut drain) = ScopedRecorder::builder().install()?;
        crate::info!(component = "index", "recorded on the recorder's thread");
        std::thread::spawn(|| crate::info!(component = "index", "recorded on another thread"))
            .join()
            .map_err(|_| "the recording thread panicked")?;
        record_scoped_signals("scoped.after");
        drop(recorder);
        let records = drain.queued_records();
        let messages = messages(&records);
        assert!(messages.contains(&"recorded on the recorder's thread"));
        assert!(messages.contains(&"scoped debug record exported"));
        assert!(messages.contains(&"recorded with scoped recorder"));
        assert!(!messages.contains(&"recorded on another thread"));
    } else if mode == "no-shutdown" {
        record_unscoped_signals("no-shutdown");
    } else if mode == "current-thread" {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async { record_unscoped_signals("current-thread") });
    } else if mode == "multi-thread" {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(async { record_unscoped_signals("multi-thread") });
    } else if mode == "unwind" {
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            record_unscoped_signals("unwind");
            panic!("the test body unwinds");
        }));
        assert!(unwind.is_err());
    } else {
        std::thread::spawn(|| record_unscoped_signals("plain.thread"))
            .join()
            .map_err(|_| "the recording thread panicked")?;
        record_unscoped_signals("plain");
    }
    if shutdown_export {
        super::shutdown_unscoped_test_export()?;
    }
    Ok(())
}

fn record_first_scoped_metrics() -> TestResult {
    let elapsed = Arc::new(AtomicU64::new(0));
    let first_clock = Arc::clone(&elapsed);
    let (first, _) = ScopedRecorder::builder()
        .clock(move || Duration::from_nanos(first_clock.load(Ordering::Relaxed)))
        .install()?;
    assert!(!super::UNSCOPED_TRIED.load(Ordering::Relaxed));
    crate::traced!("search.request", {
        elapsed.fetch_add(250_000_000, Ordering::Relaxed);
    });
    assert_scoped_metric_snapshot(&first, 1.0, 1, 0.25);
    drop(first);

    let second_clock = Arc::clone(&elapsed);
    let (second, _) = ScopedRecorder::builder()
        .clock(move || Duration::from_nanos(second_clock.load(Ordering::Relaxed)))
        .install()?;
    crate::traced!("search.request", {
        elapsed.fetch_add(250_000_000, Ordering::Relaxed);
    });
    assert_scoped_metric_snapshot(&second, 2.0, 2, 0.5);
    drop(second);
    Ok(())
}

fn assert_scoped_metric_snapshot(recorder: &ScopedRecorder, calls: f64, count: u64, sum: f64) {
    let snapshot = recorder.metrics();
    assert_eq!(
        snapshot
            .find(
                "traces.span.metrics.calls",
                &[
                    ("span.name", "search.request"),
                    ("span.kind", "Internal"),
                    ("status.code", "Ok"),
                ],
            )
            .map(|series| series.value().clone()),
        Some(SeriesValue::Sum(calls))
    );
    match snapshot
        .find(
            "traces.span.metrics.duration",
            &[
                ("span.name", "search.request"),
                ("span.kind", "Internal"),
                ("status.code", "Ok"),
            ],
        )
        .map(super::metrics::MetricSeries::value)
    {
        Some(SeriesValue::Buckets {
            count: actual_count,
            sum: actual_sum,
            ..
        }) => {
            assert_eq!(*actual_count, count);
            assert!((*actual_sum - sum).abs() < f64::EPSILON);
        }
        other => panic!("expected duration histogram, got {other:?}"),
    }
}

/// Emits one log, span, and operation metric under the unscoped subscriber.
fn record_unscoped_signals(mode: &str) {
    let span = crate::info_span!("search.request", test_mode = mode);
    span.in_scope(|| {
        crate::debug!(
            component = "test",
            test_mode = mode,
            "debug record exported"
        );
        crate::info!(
            component = "test",
            test_mode = mode,
            "recorded with no recorder"
        );
        crate::traced!("search.request", {});
    });
}

/// Emits one log, span, and operation metric under a scoped recorder.
fn record_scoped_signals(mode: &str) {
    let span = crate::info_span!("search.request", test_mode = mode);
    span.in_scope(|| {
        crate::debug!(
            component = "test",
            test_mode = mode,
            "scoped debug record exported"
        );
        crate::info!(
            component = "test",
            test_mode = mode,
            "recorded with scoped recorder"
        );
        crate::traced!("search.request", {});
    });
}

/// Starts a nextest-like child with the process exporter enabled and suppresses its
/// process output; the parent collector retains its OTLP records.
fn unscoped_child_status(
    mode: &str,
) -> Result<std::process::ExitStatus, Box<dyn std::error::Error>> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "recorder::tests::unscoped_export_child",
            "--nocapture",
        ])
        .env("RIFT_OTLP_FILTER", "debug")
        .env("OTEL_METRIC_EXPORT_INTERVAL", "600000")
        .env(SCOPED_RECORDER_STREAM_VARIABLE, "1")
        .env(UNSCOPED_CHILD_VARIABLE, mode)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    Ok(command.status()?)
}

fn unscoped_child(mode: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = unscoped_child_status(mode)?;
    // XFAIL: https://github.com/volarized/rift/issues/585
    // The original Windows arm64 plain-child 101 had no retained child stderr.
    if cfg!(all(windows, target_arch = "aarch64")) && mode == "plain" && status.code() == Some(101)
    {
        eprintln!(
            "XFAIL https://github.com/volarized/rift/issues/585: child mode {mode} exited with {status}; child stderr is retained"
        );
    } else {
        assert!(status.success(), "child mode {mode} exited with {status}");
    }
    Ok(())
}

#[test]
fn a_test_with_no_recorder_exports_from_sync_async_and_unwinding_contexts() -> TestResult {
    for mode in [
        "plain",
        "current-thread",
        "multi-thread",
        "unwind",
        "scoped",
        "no-shutdown",
        "first-scoped",
    ] {
        unscoped_child(mode)?;
    }
    Ok(())
}
