use std::pin::pin;
use std::time::Duration;

use serde_json::Value;

use super::{
    FlightEntry, FlightKind, FlightTable, OPERATIONS_IN_FLIGHT_MAX, OPERATIONS_LISTED_BYTES_MAX,
    publish_in_flight, with_table,
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
