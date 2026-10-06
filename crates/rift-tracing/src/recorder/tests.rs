use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{
    PanicOutput, RetainedRecords, SCOPED_RECORDER_PRINT_BYTES_MAX,
    SCOPED_RECORDER_PRINT_RECORDS_MAX, SCOPED_RECORDER_STREAM_VARIABLE, ScopedRecorder,
    printed_points,
};
use crate::metrics::Counter;
use crate::record::{LOG_MESSAGE_BYTES_MAX, LogRecord};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The messages `records` carries, oldest first.
fn messages(records: &[LogRecord]) -> Vec<&str> {
    records.iter().map(LogRecord::message).collect()
}

fn record(message: &str) -> LogRecord {
    LogRecord::new(1, "info", "rift_tracing::recorder", "", "", message, "{}")
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
fn a_recorder_prints_nothing_when_its_test_passes() -> TestResult {
    let printed = Arc::new(Mutex::new(String::new()));
    let (mut recorder, _drain) = ScopedRecorder::builder().install()?;
    recorder.print_into(Arc::clone(&printed));
    crate::info!("recorded and never printed");
    drop(recorder);

    assert_eq!(*printed.lock().map_err(|_| "not poisoned")?, "");
    Ok(())
}

/// Runs `work` under a recorder that prints into the returned buffer and reads a clock the
/// work moves, until `work` panics.
fn panicking_under_a_recorder(stream: bool, work: impl FnOnce(&AtomicU64)) -> String {
    let printed = Arc::new(Mutex::new(String::new()));
    let buffer = Arc::clone(&printed);
    let unwound = catch_unwind(AssertUnwindSafe(move || {
        let clock = Arc::new(AtomicU64::new(0));
        let reading = Arc::clone(&clock);
        let (mut recorder, _drain) = ScopedRecorder::builder()
            .stream(stream)
            .clock(move || Duration::from_nanos(reading.load(Ordering::Relaxed)))
            .install()
            .expect("the default filter parses");
        recorder.print_into(buffer);
        work(&clock);
        panic!("the assertion failed");
    }));
    assert!(unwound.is_err());
    printed.lock().expect("not poisoned").clone()
}

/// The panic print carries the records, then the process's metric points, one line each:
/// a sum's `value=`, a histogram's `count=`, `sum=`, and nonempty buckets.
#[test]
fn a_recorder_prints_its_records_and_metric_points_when_its_test_panics() {
    let printed = panicking_under_a_recorder(false, |clock| {
        crate::warn!(component = "index", "recorded before the panic");
        crate::traced!("test.printed", {
            clock.fetch_add(250_000_000, Ordering::Relaxed);
        });
    });

    assert!(
        printed.starts_with("scoped recorder: 2 records printed, 0 earlier records left out\n"),
        "{printed}"
    );
    assert!(printed.contains("recorded before the panic"), "{printed}");
    let (records, points) = printed
        .split_once("scoped recorder: ")
        .and_then(|(_, rest)| rest.split_once("scoped recorder: "))
        .expect("a records header, then a metric points header");
    assert!(!records.contains("metric points"), "{printed}");
    assert!(
        points.contains(" metric points printed, 0 left out\n"),
        "{printed}"
    );
    for line in [
        "traces.span.metrics.calls   span.kind=Internal span.name=test.printed \
         status.code=Ok  value=1 unit={call}",
        "traces.span.metrics.duration   span.kind=Internal span.name=test.printed \
         status.code=Ok  count=1 sum=0.25 buckets=<=0.25:1 unit=s",
    ] {
        assert!(
            points.lines().any(|printed| printed == line),
            "{line}\n{printed}"
        );
    }
}

/// A streaming recorder printed each record as it was kept, so its panic prints the
/// metric points alone.
#[test]
fn a_streaming_recorder_prints_only_its_metric_points_when_its_test_panics() {
    let printed = panicking_under_a_recorder(true, |_| {
        crate::traced!("test.streamed", {});
    });

    assert!(printed.starts_with("scoped recorder: "), "{printed}");
    assert!(
        printed.contains(" metric points printed, 0 left out\n"),
        "{printed}"
    );
    assert!(!printed.contains("records printed"), "{printed}");
    assert!(printed.contains("span.name=test.streamed"), "{printed}");
}

/// The metric points a panic prints stop at [`SCOPED_RECORDER_PRINT_BYTES_MAX`]; the
/// header counts those left out.
#[test]
fn the_print_keeps_the_metric_points_up_to_the_byte_bound() -> TestResult {
    static PRINTED: Counter<1> =
        Counter::declare("test.printed.points", "{point}", &["test.point"]);
    let (recorder, _drain) = ScopedRecorder::builder().install()?;
    let points = 2 * SCOPED_RECORDER_PRINT_BYTES_MAX / 64;
    for index in 0..points {
        let label: &'static str = Box::leak(format!("{index:0>40}").into_boxed_str());
        PRINTED.labeled([label]).add(1);
    }
    let snapshot = recorder.metrics();
    drop(recorder);

    let printed = printed_points(&snapshot);

    let (header, body) = printed.split_once('\n').ok_or("a header line")?;
    assert!(
        body.len() <= SCOPED_RECORDER_PRINT_BYTES_MAX,
        "{}",
        body.len()
    );
    let kept = body.lines().count();
    let total = snapshot.series().len();
    assert!(kept > 0 && kept < total, "{kept} of {total}");
    assert_eq!(
        header,
        format!(
            "scoped recorder: {kept} metric points printed, {} left out",
            total - kept
        )
    );
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

#[test]
fn the_print_keeps_the_newest_records_up_to_the_record_bound() {
    let retained = RetainedRecords::default();
    for index in 0..SCOPED_RECORDER_PRINT_RECORDS_MAX + 5 {
        retained.keep(&record(&format!("record {index}")));
    }

    let printed = retained.printed();

    let mut lines = printed.lines();
    assert_eq!(
        lines.next(),
        Some(
            format!(
                "scoped recorder: {SCOPED_RECORDER_PRINT_RECORDS_MAX} records printed, \
                 5 earlier records left out"
            )
            .as_str()
        )
    );
    assert!(lines.next().is_some_and(|line| line.ends_with("record 5")));
    assert!(printed.ends_with(&format!(
        "record {}\n",
        SCOPED_RECORDER_PRINT_RECORDS_MAX + 4
    )));
}

#[test]
fn the_print_keeps_the_newest_records_up_to_the_byte_bound() {
    let retained = RetainedRecords::default();
    let records = 2 * SCOPED_RECORDER_PRINT_BYTES_MAX / LOG_MESSAGE_BYTES_MAX;
    for index in 0..records {
        let message = format!("{index:>4}{}", "m".repeat(LOG_MESSAGE_BYTES_MAX - 4));
        retained.keep(&record(&message));
    }

    let printed = retained.printed();

    let (header, body) = printed.split_once('\n').expect("a header line");
    assert!(
        body.len() <= SCOPED_RECORDER_PRINT_BYTES_MAX,
        "{}",
        body.len()
    );
    let kept = body.lines().count();
    assert!(kept > 0 && kept < records, "{kept}");
    assert_eq!(
        header,
        format!(
            "scoped recorder: {kept} records printed, {} earlier records left out",
            records - kept
        )
    );
    let newest = format!("{:>4}", records - 1);
    assert!(
        body.lines()
            .last()
            .is_some_and(|line| line.contains(&format!("{newest}mmm")))
    );
}

#[test]
fn a_streaming_recorder_prints_each_record_as_it_is_kept() {
    let printed = Arc::new(Mutex::new(String::new()));
    let retained = RetainedRecords {
        stream: Some(PanicOutput::Buffer(Arc::clone(&printed))),
        ..RetainedRecords::default()
    };

    retained.keep(&record("first"));
    let after_first = printed.lock().expect("not poisoned").clone();
    retained.keep(&record("second"));

    assert_eq!(after_first, format!("{}\n", record("first").rendered()));
    assert_eq!(
        *printed.lock().expect("not poisoned"),
        format!(
            "{}\n{}\n",
            record("first").rendered(),
            record("second").rendered()
        )
    );
}

/// Selects what [`unscoped_stream_child`] records when a test starts it; unset, it records
/// nothing.
const UNSCOPED_CHILD_VARIABLE: &str = "RIFT_TRACING_UNSCOPED_CHILD";

/// The child process the unscoped stream tests start, as nextest would: `--exact` and
/// `--nocapture`. `plain` records on a spawned thread and on its own, with no recorder;
/// `scoped` records once with no recorder, then installs a recorder that does not stream,
/// records on its thread and another, and checks the drain kept its own thread's record
/// alone.
#[test]
fn unscoped_stream_child() -> TestResult {
    let Some(mode) = std::env::var_os(UNSCOPED_CHILD_VARIABLE) else {
        return Ok(());
    };
    if mode == "scoped" {
        crate::info!(component = "index", "recorded before the recorder");
        let (recorder, mut drain) = ScopedRecorder::builder().stream(false).install()?;
        crate::info!(component = "index", "recorded on the recorder's thread");
        std::thread::spawn(|| crate::info!(component = "index", "recorded on another thread"))
            .join()
            .map_err(|_| "the recording thread panicked")?;
        drop(recorder);
        assert_eq!(
            messages(&drain.queued_records()),
            ["recorded on the recorder's thread"]
        );
    } else {
        std::thread::spawn(|| crate::info!(component = "index", "recorded on another thread"))
            .join()
            .map_err(|_| "the recording thread panicked")?;
        crate::info!(component = "index", "recorded with no recorder");
    }
    Ok(())
}

/// The child's stderr, run with `mode` and with the stream variable set or not.
fn unscoped_child_stderr(mode: &str, stream: bool) -> Result<String, Box<dyn std::error::Error>> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "recorder::tests::unscoped_stream_child",
            "--nocapture",
        ])
        .env(UNSCOPED_CHILD_VARIABLE, mode);
    if stream {
        command.env(SCOPED_RECORDER_STREAM_VARIABLE, "1");
    } else {
        command.env_remove(SCOPED_RECORDER_STREAM_VARIABLE);
    }
    let output = command.output()?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(output.status.success(), "{stderr}");
    Ok(stderr)
}

#[test]
fn a_test_with_no_recorder_streams_every_thread_under_the_variable() -> TestResult {
    let stderr = unscoped_child_stderr("plain", true)?;
    assert!(stderr.contains("recorded on another thread"), "{stderr}");
    assert!(stderr.contains("recorded with no recorder"), "{stderr}");
    Ok(())
}

/// The stream prints the record made before the recorder; the recorder then takes its own
/// thread's record, and the stream, stopped at the install, prints the other thread's
/// record no more.
#[test]
fn a_recorder_takes_its_thread_and_stops_the_unscoped_stream() -> TestResult {
    let stderr = unscoped_child_stderr("scoped", true)?;
    assert!(stderr.contains("recorded before the recorder"), "{stderr}");
    assert!(!stderr.contains("recorded on"), "{stderr}");
    Ok(())
}

#[test]
fn a_test_without_the_variable_streams_nothing() -> TestResult {
    let stderr = unscoped_child_stderr("plain", false)?;
    assert!(!stderr.contains("recorded"), "{stderr}");
    Ok(())
}
