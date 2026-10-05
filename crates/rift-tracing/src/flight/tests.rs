use std::pin::pin;
use std::time::Duration;

use serde_json::Value;

use super::{
    FlightEntry, FlightKind, FlightTable, OPERATIONS_IN_FLIGHT_MAX, OPERATIONS_LISTED_BYTES_MAX,
    publish_in_flight, warn_in_flight, with_table,
};
use crate::{LogRecord, ScopedRecorder};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// An operation entry named `name`, opened `started` after the clock's epoch.
fn entry(name: &'static str, started: Duration) -> FlightEntry {
    FlightEntry::opened(name, FlightKind::Operation, None, started, 0)
}

/// The names the table lists, oldest first.
fn listed_names(table: &FlightTable, now: Duration) -> Result<Vec<String>, serde_json::Error> {
    let listing: Value = serde_json::from_str(&table.listing(now).operations)?;
    Ok(listing
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["operation"].as_str().map(str::to_owned))
        .collect())
}

/// The records `messages` names, in order.
fn with_message<'records>(
    records: &'records [LogRecord],
    message: &str,
) -> Vec<&'records LogRecord> {
    records
        .iter()
        .filter(|record| record.message() == message)
        .collect()
}

/// The `operations` list of a table record, parsed.
fn operations(record: &LogRecord) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let fields: Value = serde_json::from_str(record.fields())?;
    let listed = fields["operations"]
        .as_str()
        .ok_or("a table record lists operations")?;
    let listed: Value = serde_json::from_str(listed)?;
    Ok(listed.as_array().cloned().unwrap_or_default())
}

#[test]
fn an_entry_joins_on_open_and_leaves_on_close() -> TestResult {
    let (recorder, _drain) = ScopedRecorder::builder().install()?;
    let inside = crate::traced!(component = "lexical", operation = "lexical.commit", {
        crate::traced!("lexical.documents", {
            with_table(|table| table.listing(Duration::MAX))
        })
    })
    .ok_or("the recorder keeps a table")?;
    let after = with_table(|table| table.listing(Duration::MAX)).ok_or("the table stays")?;
    drop(recorder);

    let listed: Value = serde_json::from_str(&inside.operations)?;
    assert_eq!(inside.in_flight, 2);
    assert_eq!(listed[0]["operation"], "lexical.commit");
    assert_eq!(listed[0]["component"], "lexical");
    assert_eq!(listed[0]["kind"], "operation");
    assert_eq!(listed[1]["operation"], "lexical.documents");
    assert_eq!(listed[1]["parent"], "lexical.commit");
    assert_eq!(after.in_flight, 0, "a closed operation leaves the table");
    Ok(())
}

#[test]
fn a_span_without_an_operation_stays_out_and_one_below_the_capture_filter_joins() -> TestResult {
    let (recorder, _drain) = ScopedRecorder::builder().capture("off").install()?;
    let span = crate::info_span!("cloud.request", status = 0_u16);
    let listing = span
        .in_scope(|| {
            crate::traced!("index.parse", {
                with_table(|table| table.listing(Duration::MAX))
            })
        })
        .ok_or("the recorder keeps a table")?;
    drop(recorder);

    assert_eq!(
        listing.in_flight, 1,
        "the span without an operation is no entry"
    );
    let listed: Value = serde_json::from_str(&listing.operations)?;
    assert_eq!(listed[0]["operation"], "index.parse");
    assert_eq!(
        listed[0]["parent"], "cloud.request",
        "it still names a parent"
    );
    Ok(())
}

#[test]
fn a_full_table_counts_each_refused_entry_once() {
    let table = FlightTable::default();
    for identity in 0..=OPERATIONS_IN_FLIGHT_MAX as u64 {
        table.join(identity, entry("index.parse", Duration::ZERO));
    }
    let listing = table.listing(Duration::ZERO);
    assert_eq!(listing.in_flight, OPERATIONS_IN_FLIGHT_MAX as u64);
    assert_eq!(listing.untracked, 1);

    table.leave(0);
    table.join(
        OPERATIONS_IN_FLIGHT_MAX as u64 + 1,
        entry("index.parse", Duration::ZERO),
    );
    let listing = table.listing(Duration::ZERO);
    assert_eq!(listing.in_flight, OPERATIONS_IN_FLIGHT_MAX as u64);
    assert_eq!(listing.untracked, 1, "a join with room is not counted");
}

#[test]
fn a_listing_is_oldest_first_and_counts_what_its_bound_left_out() -> TestResult {
    let table = FlightTable::default();
    table.join(1, entry("index.second", Duration::from_secs(2)));
    table.join(2, entry("index.first", Duration::from_secs(1)));
    assert_eq!(
        listed_names(&table, Duration::from_secs(3))?,
        ["index.first", "index.second"]
    );

    let crowded = FlightTable::default();
    for identity in 0..200_u64 {
        let mut long = entry("index.parse", Duration::from_secs(identity));
        long.component = "c".repeat(64);
        crowded.join(identity, long);
    }
    let listing = crowded.listing(Duration::from_secs(300));
    assert!(listing.operations.len() <= OPERATIONS_LISTED_BYTES_MAX);
    let listed: Value = serde_json::from_str(&listing.operations)?;
    let shown = listed.as_array().map_or(0, Vec::len) as u64;
    assert!(shown > 0 && listing.left_out > 0);
    assert_eq!(shown + listing.left_out, 200);
    assert_eq!(
        listed[0]["age_ms"], 300_000,
        "the oldest entry is listed first"
    );
    Ok(())
}

#[test]
fn a_stalled_entry_is_reported_once() {
    let table = FlightTable::default();
    table.join(1, entry("lexical.commit", Duration::from_secs(1)));
    table.join(2, entry("search.request", Duration::from_secs(9)));
    let delay = Duration::from_secs(10);

    assert_eq!(table.stalled(Duration::from_secs(10), delay), None);
    let first = table
        .stalled(Duration::from_secs(11), delay)
        .expect("one entry passed the delay");
    assert_eq!(first.in_flight, 1);
    assert!(first.operations.contains("lexical.commit"));
    assert_eq!(
        table.stalled(Duration::from_secs(12), delay),
        None,
        "reported once"
    );
    let second = table
        .stalled(Duration::from_secs(19), delay)
        .expect("the second entry passed the delay");
    assert!(second.operations.contains("search.request"));
    assert!(!second.operations.contains("lexical.commit"));
}

#[test]
fn a_published_table_names_the_open_operations_and_the_reason() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    crate::traced!(component = "mcp", operation = "server.stop", {
        crate::traced!(component = "lexical", operation = "lexical.commit", {
            publish_in_flight("stop");
        });
    });
    drop(recorder);

    let records = drain.queued_records();
    let published = with_message(&records, "operations in flight");
    assert_eq!(published.len(), 1);
    let record = published[0];
    assert_eq!(record.target(), "rift_tracing::flight");
    assert_eq!(record.level(), "info");
    let fields: Value = serde_json::from_str(record.fields())?;
    assert_eq!(fields["reason"], "stop");
    assert_eq!(fields["in_flight"], "2");
    assert_eq!(fields["left_out"], "0");
    assert_eq!(fields["untracked"], "0");
    let listed = operations(record)?;
    assert_eq!(listed[0]["operation"], "server.stop");
    assert_eq!(listed[1]["operation"], "lexical.commit");
    assert_eq!(listed[1]["parent"], "server.stop");
    Ok(())
}

/// The warning form publishes the same table at `WARN`, so a filter that admits warnings
/// alone keeps it, and an entry whose span names its `work` lists it.
#[test]
fn a_warned_table_is_a_warning_and_lists_the_work_of_an_entry() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().capture("warn").install()?;
    let worker = crate::debug_span!(
        "worker.run",
        component = "worker",
        operation = "worker.run",
        work = "get_symbol"
    );
    worker.in_scope(|| warn_in_flight("stop deadline"));
    drop(worker);
    drop(recorder);

    let records = drain.queued_records();
    let published = with_message(&records, "operations in flight");
    assert_eq!(published.len(), 1, "{records:?}");
    assert_eq!(published[0].level(), "warn");
    let fields: Value = serde_json::from_str(published[0].fields())?;
    assert_eq!(fields["reason"], "stop deadline");
    let listed = operations(published[0])?;
    assert_eq!(listed[0]["operation"], "worker.run");
    assert_eq!(listed[0]["work"], "get_symbol");
    Ok(())
}

#[test]
fn publishing_without_a_table_records_nothing() {
    publish_in_flight("stop");
    assert!(with_table(|table| table.listing(Duration::ZERO)).is_none());
}

#[test]
fn an_open_operation_records_its_opening_inside_its_span() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let documents = 3;
    let parsed = crate::traced!(
        component = "lexical",
        operation = "lexical.commit",
        open = true,
        documents = documents,
        { documents + 1 }
    );
    drop(recorder);

    assert_eq!(parsed, 4);
    let records = drain.queued_records();
    let opened = with_message(&records, "operation opened");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].target(), "rift_tracing::flight");
    assert_eq!(opened[0].component(), "lexical");
    assert_eq!(opened[0].operation(), "lexical.commit");
    let fields: Value = serde_json::from_str(opened[0].fields())?;
    assert_eq!(fields["root_span"]["fields"]["documents"], "3");
    let opened_at = records
        .iter()
        .position(|record| record.message() == "operation opened");
    let closed_at = records
        .iter()
        .position(|record| record.message() == "lexical.commit");
    assert!(opened_at < closed_at, "the opening precedes the close");
    Ok(())
}

#[tokio::test]
async fn an_awaited_open_operation_records_its_opening_at_the_first_poll() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let mut commit = pin!(crate::traced!(
        component = "lexical",
        operation = "lexical.commit",
        open = true,
        async move { released.await.is_ok() }
    ));
    assert!(
        drain.queued_records().is_empty(),
        "a future not yet polled opens nothing"
    );
    tokio::select! {
        biased;
        _ = commit.as_mut() => return Err("the commit waits for its release".into()),
        () = std::future::ready(()) => {}
    }
    let opened = drain.queued_records();
    assert_eq!(
        opened.len(),
        1,
        "the opening is recorded before the work ends"
    );
    assert_eq!(opened[0].message(), "operation opened");
    assert_eq!(opened[0].operation(), "lexical.commit");
    let in_flight = with_table(|table| table.listing(Duration::MAX)).ok_or("a table")?;
    assert!(in_flight.operations.contains("lexical.commit"));

    release.send(()).map_err(|()| "the commit still waits")?;
    assert!(commit.await);
    drop(recorder);
    Ok(())
}

/// A published table lists a lifelong hold with its mark beside the open operations, so
/// a reader of a stop or timeout record tells it from stuck work.
#[test]
fn a_published_table_marks_a_lifelong_hold() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let live = std::sync::Mutex::new(());
    let live_hold = crate::traced!("history.open", {
        crate::lock("history.live")
            .shared()
            .lifelong()
            .try_acquire(|| live.try_lock())
    })
    .map_err(|_| "the lock is free")?;
    crate::traced!(component = "mcp", operation = "server.stop", {
        crate::publish_in_flight("stop");
    });
    drop(live_hold);
    drop(recorder);

    let records = drain.queued_records();
    let published = with_message(&records, "operations in flight");
    assert_eq!(published.len(), 1);
    let listed = operations(published[0])?;
    let listed_hold = listed
        .iter()
        .find(|entry| entry["kind"] == "held")
        .ok_or("the lifelong hold is listed")?;
    assert_eq!(listed_hold["lifelong"], true);
    assert_eq!(listed_hold["lock.name"], "history.live");
    let stop = listed
        .iter()
        .find(|entry| entry["operation"] == "server.stop")
        .ok_or("the stop is listed")?;
    assert!(
        stop.get("lifelong").is_none(),
        "an operation carries no mark"
    );
    Ok(())
}

/// An operation opened as a root inside another, such as work detached from the request
/// that started it, lists no parent: only a lifelong hold names the span it was opened in.
#[test]
fn a_root_operation_opened_inside_another_lists_no_parent() -> TestResult {
    let (recorder, _drain) = ScopedRecorder::builder().install()?;
    let listing = crate::traced!("search.request", {
        let detached =
            tracing::info_span!(parent: None, "index.reconcile", operation = "index.reconcile");
        let listing = with_table(|table| table.listing(Duration::MAX));
        drop(detached);
        listing
    })
    .ok_or("the recorder keeps a table")?;
    drop(recorder);

    let listed: Value = serde_json::from_str(&listing.operations)?;
    let detached = listed
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["operation"] == "index.reconcile")
        })
        .ok_or("the root operation is listed")?;
    assert!(detached.get("parent").is_none(), "{detached}");
    Ok(())
}

/// The stall report ticks at a quarter of `stall_delay`, never under a quarter second and
/// never over five seconds.
#[test]
fn the_stall_tick_is_a_quarter_of_the_delay_within_its_bounds() {
    use super::{STALL_TICK_MAX, STALL_TICK_MIN, stall_tick};
    assert_eq!(stall_tick(Duration::from_secs(1)), STALL_TICK_MIN);
    assert_eq!(stall_tick(Duration::from_millis(100)), STALL_TICK_MIN);
    assert_eq!(
        stall_tick(Duration::from_secs(10)),
        Duration::from_millis(2_500)
    );
    assert_eq!(stall_tick(Duration::from_secs(20)), STALL_TICK_MAX);
    assert_eq!(stall_tick(Duration::from_secs(3_600)), STALL_TICK_MAX);
}

/// The stall reports `drain` holds now.
fn stall_reports(drain: &mut crate::LogDrain) -> Vec<LogRecord> {
    drain
        .queued_records()
        .into_iter()
        .filter(|record| record.message() == "operations in flight past the stall delay")
        .collect()
}

/// Lets the stall report's task run every tick the paused clock reached.
async fn settle() {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

/// An entry is reported once, on the first tick at which it has been open for
/// `stall_delay`; the ticks before and after report nothing. A lifelong hold beside it is
/// never reported.
#[tokio::test(start_paused = true)]
async fn the_stall_report_reports_each_entry_once_on_its_own_tick() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let table = std::sync::Arc::new(FlightTable::default());
    let mut held = FlightEntry::opened(
        "lock.held",
        FlightKind::Held,
        Some("history.open"),
        Duration::ZERO,
        0,
    );
    held.lock = "history.live".to_owned();
    held.lifelong = true;
    table.join(1, held);
    table.join(2, entry("lexical.commit", Duration::ZERO));
    let started = tokio::time::Instant::now();
    let stall_delay = Duration::from_secs(4);
    let tick = super::stall_tick(stall_delay);
    assert_eq!(tick, Duration::from_secs(1));
    let report = super::StallReport::spawn_with_clock(
        std::sync::Arc::clone(&table),
        stall_delay,
        move || started.elapsed(),
    );
    settle().await;
    for second in 1..4 {
        tokio::time::advance(tick).await;
        settle().await;
        assert!(
            stall_reports(&mut drain).is_empty(),
            "nothing is open past the delay at {second}s"
        );
    }
    tokio::time::advance(tick).await;
    settle().await;
    let reported = stall_reports(&mut drain);
    assert_eq!(reported.len(), 1, "the tick at the delay reports the entry");
    assert_eq!(reported[0].level(), "warn");
    let fields: Value = serde_json::from_str(reported[0].fields())?;
    assert_eq!(fields["reason"], "stall_delay");
    assert_eq!(fields["in_flight"], "1");
    assert!(reported[0].fields().contains("lexical.commit"));
    assert!(!reported[0].fields().contains("history.live"));
    for _ in 0..3 {
        tokio::time::advance(tick).await;
        settle().await;
    }
    assert!(
        stall_reports(&mut drain).is_empty(),
        "an entry is reported once"
    );
    report.stop().await;
    drop(recorder);
    Ok(())
}

/// `operation.active` reports the entries open when the meter collects, by span name, and
/// none once they close.
#[test]
fn operation_active_reports_the_entries_open_at_collection() {
    let (recorder, _records) = ScopedRecorder::builder()
        .install()
        .expect("the default filter parses");
    let labels = [("span.name", "test.active")];
    let inside = crate::traced!(component = "test", operation = "test.active", {
        recorder.metrics()
    });
    let series = inside
        .find("operation.active", &labels)
        .expect("the open operation is reported");
    assert_eq!(series.unit(), "{operation}");
    assert_eq!(series.labels(), labels);
    assert_eq!(series.value(), &crate::SeriesValue::Sum(1.0));

    let after = recorder.metrics();
    assert_eq!(
        after.find("operation.active", &labels),
        None,
        "a closed operation is no longer reported"
    );
}

/// An operation that finds the table full adds one to `operation.untracked`, and the
/// table's own entries are what `operation.active` reports.
#[test]
fn a_full_table_counts_the_refused_entry_in_operation_untracked() {
    let (recorder, _records) = ScopedRecorder::builder()
        .install()
        .expect("the default filter parses");
    let open: Vec<tracing::Span> = (0..=OPERATIONS_IN_FLIGHT_MAX)
        .map(|_| tracing::info_span!("test.filled", operation = "test.filled"))
        .collect();

    let snapshot = recorder.metrics();
    let untracked = snapshot
        .find("operation.untracked", &[])
        .expect("the refused entry is counted");
    assert_eq!(untracked.unit(), "{operation}");
    assert_eq!(untracked.value(), &crate::SeriesValue::Sum(1.0));
    #[expect(
        clippy::cast_precision_loss,
        reason = "the table bound converts exactly"
    )]
    let tracked = OPERATIONS_IN_FLIGHT_MAX as f64;
    assert_eq!(
        snapshot
            .find("operation.active", &[("span.name", "test.filled")])
            .map(|series| series.value().clone()),
        Some(crate::SeriesValue::Sum(tracked))
    );
    drop(open);
}
