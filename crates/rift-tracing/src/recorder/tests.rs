use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use super::{
    RetainedRecords, SCOPED_RECORDER_PRINT_BYTES_MAX, SCOPED_RECORDER_PRINT_RECORDS_MAX,
    ScopedRecorder,
};
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

#[test]
fn a_recorder_prints_its_records_when_its_test_panics() {
    let printed = Arc::new(Mutex::new(String::new()));
    let buffer = Arc::clone(&printed);

    let unwound = catch_unwind(AssertUnwindSafe(move || {
        let (mut recorder, _drain) = ScopedRecorder::builder()
            .install()
            .expect("the default filter parses");
        recorder.print_into(buffer);
        crate::warn!(component = "index", "recorded before the panic");
        panic!("the assertion failed");
    }));

    assert!(unwound.is_err());
    let printed = printed.lock().expect("not poisoned").clone();
    assert!(
        printed.starts_with("scoped recorder: 1 records printed, 0 earlier records left out\n"),
        "{printed}"
    );
    assert!(printed.contains("recorded before the panic"), "{printed}");
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
