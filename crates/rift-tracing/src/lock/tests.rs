use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use super::{Refusal, lock};
use crate::flight::with_table;
use crate::{LogRecord, MetricSnapshot, ScopedRecorder, SeriesValue};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The close records of the lock spans named `name`, in order.
fn closed<'records>(records: &'records [LogRecord], name: &str) -> Vec<&'records LogRecord> {
    records
        .iter()
        .filter(|record| {
            record.message() == name && record.fields().contains("\"span\":\"closed\"")
        })
        .collect()
}

/// One record's fields, parsed.
fn fields(record: &LogRecord) -> Result<Value, serde_json::Error> {
    serde_json::from_str(record.fields())
}

/// The count of the histogram series `name` with `labels`.
fn count(metrics: &MetricSnapshot, name: &str, labels: &[(&str, &str)]) -> u64 {
    match metrics.find(name, labels).map(crate::MetricSeries::value) {
        Some(SeriesValue::Buckets { count, .. }) => *count,
        _ => 0,
    }
}

/// Polls `future` once, answering whether it was still pending.
async fn pending_after_one_poll<Work: Future + Unpin>(future: &mut Work) -> bool {
    tokio::select! {
        biased;
        _ = future => false,
        () = std::future::ready(()) => true,
    }
}

#[tokio::test]
async fn an_uncontended_acquisition_records_no_wait_and_holds_until_dropped() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Mutex::new(());
    let turn = lock("index.write").acquire(writes.lock()).await;
    let held = with_table(|table| table.holder_of("index.write")).flatten();
    assert!(
        writes.try_lock().is_err(),
        "the wrapped guard holds the lock"
    );
    drop(turn);
    assert!(
        writes.try_lock().is_ok(),
        "dropping the held lock releases it"
    );
    let metrics = recorder.metrics();
    drop(recorder);

    assert_eq!(
        held, None,
        "a lock taken outside any operation names no holder"
    );
    let records = drain.queued_records();
    assert!(
        closed(&records, "lock.wait").is_empty(),
        "no wait span without a wait"
    );
    let held = closed(&records, "lock.held");
    assert_eq!(held.len(), 1);
    assert_eq!(
        held[0].level(),
        "debug",
        "an uncontended hold stays below info"
    );
    let labels = [("lock.name", "index.write"), ("lock.mode", "exclusive")];
    assert_eq!(count(&metrics, "lock.wait.duration", &labels), 1);
    assert_eq!(count(&metrics, "lock.held.duration", &labels), 1);
    Ok(())
}

#[tokio::test]
async fn a_contended_wait_names_its_waiter_and_holder_and_ends_acquired() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Mutex::new(());
    let holder = crate::traced!("documentation.write", async {
        lock("index.write").acquire(writes.lock()).await
    })
    .await;
    let mut waiter = pin!(crate::traced!("lexical.commit", async {
        lock("index.write").acquire(writes.lock()).await
    }));
    assert!(
        pending_after_one_poll(&mut waiter).await,
        "the holder keeps the lock"
    );
    let in_flight = with_table(|table| table.listing(Duration::MAX)).ok_or("a table")?;
    let listed: Value = serde_json::from_str(&in_flight.operations)?;
    let kinds: Vec<(&str, &str)> = listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| Some((entry["kind"].as_str()?, entry["parent"].as_str()?)))
        .collect();
    assert!(
        kinds.contains(&("held", "documentation.write")),
        "{kinds:?}"
    );
    assert!(kinds.contains(&("wait", "lexical.commit")), "{kinds:?}");

    drop(holder);
    let turn = waiter.await;
    drop(turn);
    let metrics = recorder.metrics();
    drop(recorder);

    let records = drain.queued_records();
    let waits = closed(&records, "lock.wait");
    assert_eq!(waits.len(), 1);
    let wait = fields(waits[0])?;
    assert_eq!(waits[0].level(), "info");
    assert_eq!(waits[0].target(), "rift_tracing::lock");
    assert_eq!(wait["lock.name"], "index.write");
    assert_eq!(wait["lock.mode"], "exclusive");
    assert_eq!(wait["waiter"], "lexical.commit");
    assert_eq!(wait["holder"], "documentation.write");
    assert_eq!(wait["outcome"], "acquired");
    let holds = closed(&records, "lock.held");
    assert_eq!(holds.len(), 2);
    assert_eq!(
        holds[1].level(),
        "info",
        "a contended hold reaches info beside its wait"
    );
    assert_eq!(fields(holds[1])?["holder"], "lexical.commit");
    let labels = [("lock.name", "index.write"), ("lock.mode", "exclusive")];
    assert_eq!(count(&metrics, "lock.wait.duration", &labels), 2);
    assert_eq!(count(&metrics, "lock.held.duration", &labels), 2);
    Ok(())
}

#[tokio::test]
async fn a_wait_dropped_before_it_acquires_ends_cancelled_and_leaves_the_queue() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Mutex::new(());
    let holder = lock("index.write").acquire(writes.lock()).await;
    {
        let mut waiter = pin!(lock("index.write").acquire(writes.lock()));
        assert!(pending_after_one_poll(&mut waiter).await);
    }
    drop(holder);
    assert!(
        writes.try_lock().is_ok(),
        "the cancelled waiter left the queue"
    );
    let metrics = recorder.metrics();
    drop(recorder);

    let records = drain.queued_records();
    let waits = closed(&records, "lock.wait");
    assert_eq!(waits.len(), 1);
    assert_eq!(fields(waits[0])?["outcome"], "cancelled");
    let cancelled = [
        ("lock.name", "index.write"),
        ("lock.mode", "exclusive"),
        ("error.type", "cancelled"),
    ];
    assert_eq!(count(&metrics, "lock.wait.duration", &cancelled), 1);
    Ok(())
}

#[tokio::test]
async fn a_future_dropped_before_its_first_poll_records_nothing() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Mutex::new(());
    drop(lock("index.write").acquire(writes.lock()));
    let metrics = recorder.metrics();
    drop(recorder);

    assert!(drain.queued_records().is_empty());
    assert!(
        metrics
            .find("lock.wait.duration", &[("lock.name", "index.write")])
            .is_none()
    );
    assert!(metrics.series().is_empty());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_wait_past_its_timeout_ends_timeout() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Mutex::new(());
    let holder = lock("index.write").acquire(writes.lock()).await;
    let waited = lock("index.write")
        .acquire_within(Duration::from_secs(1), writes.lock())
        .await;
    drop(holder);
    let metrics = recorder.metrics();
    drop(recorder);

    assert!(waited.is_err(), "the holder kept the lock past the timeout");
    let records = drain.queued_records();
    let waits = closed(&records, "lock.wait");
    assert_eq!(waits.len(), 1);
    assert_eq!(fields(waits[0])?["outcome"], "timeout");
    let timeout = [
        ("lock.name", "index.write"),
        ("lock.mode", "exclusive"),
        ("error.type", "timeout"),
    ];
    assert_eq!(count(&metrics, "lock.wait.duration", &timeout), 1);
    Ok(())
}

#[tokio::test]
async fn a_wait_within_its_timeout_acquires() -> TestResult {
    let writes = Mutex::new(());
    let turn = lock("index.write")
        .acquire_within(Duration::from_secs(1), writes.lock())
        .await?;
    assert!(writes.try_lock().is_err());
    drop(turn);
    Ok(())
}

#[test]
fn a_refused_attempt_ends_refused_and_names_the_holder() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let election = std::sync::Mutex::new(());
    let claim = crate::traced!("election.claim", {
        lock("election").try_acquire(|| election.try_lock())
    });
    let claim = claim.map_err(|_| "the first claim is free")?;
    let refused = crate::traced!("election.probe", {
        lock("election")
            .try_acquire(|| election.try_lock())
            .is_err()
    });
    drop(claim);
    let metrics = recorder.metrics();
    drop(recorder);

    assert!(refused, "the held claim refuses a second");
    let records = drain.queued_records();
    let waits = closed(&records, "lock.wait");
    assert_eq!(waits.len(), 1);
    let wait = fields(waits[0])?;
    assert_eq!(wait["outcome"], "refused");
    assert_eq!(wait["waiter"], "election.probe");
    assert_eq!(wait["holder"], "election.claim");
    let refused = [
        ("lock.name", "election"),
        ("lock.mode", "exclusive"),
        ("error.type", "refused"),
    ];
    assert_eq!(count(&metrics, "lock.wait.duration", &refused), 1);
    Ok(())
}

#[tokio::test]
async fn read_and_write_acquisitions_record_apart() -> TestResult {
    let (recorder, _drain) = ScopedRecorder::builder().install()?;
    let snapshot = RwLock::new(1_u8);
    let read = lock("snapshot").shared().acquire(snapshot.read()).await;
    assert_eq!(**read, 1);
    drop(read);
    let mut write = lock("snapshot").exclusive().acquire(snapshot.write()).await;
    **write = 2;
    drop(write);
    let metrics = recorder.metrics();
    drop(recorder);

    for mode in ["shared", "exclusive"] {
        let labels = [("lock.name", "snapshot"), ("lock.mode", mode)];
        assert_eq!(count(&metrics, "lock.held.duration", &labels), 1, "{mode}");
    }
    assert_eq!(*snapshot.read().await, 2);
    Ok(())
}

#[tokio::test]
async fn a_lock_held_across_an_await_stays_in_flight_until_dropped() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Mutex::new(());
    let turn = crate::traced!("lexical.commit", async {
        let turn = lock("index.write").acquire(writes.lock()).await;
        tokio::task::yield_now().await;
        turn
    })
    .await;
    let holder = with_table(|table| table.holder_of("index.write")).flatten();
    drop(turn);
    let after = with_table(|table| table.holder_of("index.write")).flatten();
    drop(recorder);

    assert_eq!(
        holder,
        Some("lexical.commit"),
        "the hold outlives the operation that took it"
    );
    assert_eq!(after, None);
    assert_eq!(closed(&drain.queued_records(), "lock.held").len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_held_lock_moved_into_a_blocking_closure_records_its_hold_there() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Arc::new(Mutex::new(()));
    let turn = lock("index.write")
        .acquire(Arc::clone(&writes).lock_owned())
        .await;
    tokio::task::spawn_blocking(move || drop(turn)).await?;
    assert!(writes.try_lock().is_ok(), "the closure released the lock");
    let metrics = recorder.metrics();
    drop(recorder);

    assert_eq!(closed(&drain.queued_records(), "lock.held").len(), 1);
    let labels = [("lock.name", "index.write"), ("lock.mode", "exclusive")];
    assert_eq!(
        count(&metrics, "lock.held.duration", &labels),
        1,
        "the hold records through the dispatcher it was taken under"
    );
    Ok(())
}

#[tokio::test]
async fn without_a_subscriber_a_lock_still_locks_and_records_nothing() {
    let writes = Mutex::new(0_u8);
    let mut turn = lock("index.write").acquire(writes.lock()).await;
    **turn += 1;
    drop(turn);
    assert_eq!(*writes.lock().await, 1);
    assert!(with_table(|table| table.holder_of("index.write")).is_none());
}

#[tokio::test]
async fn a_fallible_wait_refused_at_its_first_poll_ends_refused_and_holds_nothing() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let refused = crate::traced!("database.open", async {
        lock("index.migration")
            .acquire_fallible(async { Err::<(), _>(Refusal::Refused("locked elsewhere")) })
            .await
    })
    .await;
    let metrics = recorder.metrics();
    drop(recorder);

    assert_eq!(refused.err(), Some("locked elsewhere"));
    let records = drain.queued_records();
    let waits = closed(&records, "lock.wait");
    assert_eq!(waits.len(), 1, "a refusal leaves a wait record");
    let wait = fields(waits[0])?;
    assert_eq!(wait["outcome"], "refused");
    assert_eq!(wait["waiter"], "database.open");
    assert!(closed(&records, "lock.held").is_empty(), "no hold");
    let refused = [
        ("lock.name", "index.migration"),
        ("lock.mode", "exclusive"),
        ("error.type", "refused"),
    ];
    assert_eq!(count(&metrics, "lock.wait.duration", &refused), 1);
    let labels = [("lock.name", "index.migration"), ("lock.mode", "exclusive")];
    assert_eq!(count(&metrics, "lock.held.duration", &labels), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_fallible_wait_that_runs_out_its_budget_ends_timeout_at_the_budget() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let budget = Duration::from_millis(1_000);
    let started = tokio::time::Instant::now();
    let mut attempts = 0_u32;
    let timed_out = lock("index.migration")
        .acquire_fallible(async {
            let deadline = started + budget;
            loop {
                attempts += 1;
                if tokio::time::Instant::now() >= deadline {
                    return Err::<(), _>(Refusal::Timeout("budget spent"));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    let waited = started.elapsed();
    let metrics = recorder.metrics();
    drop(recorder);

    assert_eq!(timed_out.err(), Some("budget spent"));
    assert_eq!(waited, budget, "the wait ends at the attempt on the budget");
    assert_eq!(
        attempts, 101,
        "one attempt per poll, the last on the budget"
    );
    let records = drain.queued_records();
    let waits = closed(&records, "lock.wait");
    assert_eq!(waits.len(), 1);
    assert_eq!(fields(waits[0])?["outcome"], "timeout");
    assert!(closed(&records, "lock.held").is_empty(), "no hold");
    let timeout = [
        ("lock.name", "index.migration"),
        ("lock.mode", "exclusive"),
        ("error.type", "timeout"),
    ];
    match metrics
        .find("lock.wait.duration", &timeout)
        .map(crate::MetricSeries::value)
    {
        Some(SeriesValue::Buckets { count, .. }) => assert_eq!(*count, 1),
        other => return Err(format!("no timeout series: {other:?}").into()),
    }
    Ok(())
}

#[tokio::test]
async fn a_fallible_wait_that_acquires_after_waiting_holds_like_any_other() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Mutex::new(());
    let holder = lock("index.write").acquire(writes.lock()).await;
    let mut waiter = pin!(crate::traced!("lexical.commit", async {
        lock("index.write")
            .acquire_fallible(async { Ok::<_, Refusal<()>>(writes.lock().await) })
            .await
    }));
    assert!(pending_after_one_poll(&mut waiter).await);
    drop(holder);
    let turn = waiter.await.map_err(|()| "the wait acquires")?;
    assert!(writes.try_lock().is_err(), "the guard holds the lock");
    drop(turn);
    assert!(writes.try_lock().is_ok(), "the guard drops with the hold");
    drop(recorder);

    let records = drain.queued_records();
    let waits = closed(&records, "lock.wait");
    assert_eq!(waits.len(), 1);
    assert_eq!(fields(waits[0])?["outcome"], "acquired");
    let holds = closed(&records, "lock.held");
    assert_eq!(holds.len(), 2);
    assert_eq!(holds[1].level(), "info", "a contended hold reaches info");
    assert_eq!(fields(holds[1])?["holder"], "lexical.commit");
    Ok(())
}

#[tokio::test]
async fn a_fallible_wait_dropped_while_pending_ends_cancelled() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Mutex::new(());
    let holder = lock("index.write").acquire(writes.lock()).await;
    {
        let mut waiter = pin!(
            lock("index.write")
                .acquire_fallible(async { Ok::<_, Refusal<()>>(writes.lock().await) })
        );
        assert!(pending_after_one_poll(&mut waiter).await);
    }
    drop(holder);
    assert!(
        writes.try_lock().is_ok(),
        "the cancelled waiter left the queue"
    );
    drop(recorder);

    let records = drain.queued_records();
    let waits = closed(&records, "lock.wait");
    assert_eq!(waits.len(), 1);
    assert_eq!(fields(waits[0])?["outcome"], "cancelled");
    assert_eq!(closed(&records, "lock.held").len(), 1, "the holder's alone");
    Ok(())
}

#[test]
fn a_lifelong_hold_is_marked_in_its_span_and_the_table() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let live = std::sync::Mutex::new(());
    let held = crate::traced!("history.open", {
        lock("history.live")
            .shared()
            .lifelong()
            .try_acquire(|| live.try_lock())
    })
    .map_err(|_| "the lock is free")?;
    let listing = with_table(|table| table.listing(Duration::MAX)).ok_or("a table")?;
    let stalled =
        with_table(|table| table.stalled(Duration::MAX, Duration::ZERO)).ok_or("a table")?;
    drop(held);
    drop(recorder);

    let listed: Value = serde_json::from_str(&listing.operations)?;
    let entry = listed
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["kind"] == "held"))
        .ok_or("the hold is listed")?;
    assert_eq!(entry["lifelong"], true);
    assert_eq!(entry["lock.name"], "history.live");
    assert_eq!(
        entry["parent"], "history.open",
        "the hold names the operation that took it"
    );
    assert_eq!(
        listing.in_flight, 1,
        "the operation that took the lock closed with its work: {listed}"
    );
    assert!(
        stalled.is_none(),
        "a lifelong hold is never reported stalled: {stalled:?}"
    );
    let holds = closed(&drain.queued_records(), "lock.held")
        .into_iter()
        .map(fields)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0]["lifelong"], "true", "stored fields are text");
    assert_eq!(holds[0]["holder"], "history.open");
    Ok(())
}

#[test]
fn a_hold_not_declared_lifelong_carries_no_mark() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = std::sync::Mutex::new(());
    let held = lock("index.write")
        .try_acquire(|| writes.try_lock())
        .map_err(|_| "the lock is free")?;
    let listing = with_table(|table| table.listing(Duration::MAX)).ok_or("a table")?;
    drop(held);
    drop(recorder);

    let listed: Value = serde_json::from_str(&listing.operations)?;
    assert_eq!(listed[0]["kind"], "held");
    assert!(listed[0].get("lifelong").is_none(), "{listed}");
    let records = drain.queued_records();
    let holds = closed(&records, "lock.held");
    assert!(fields(holds[0])?.get("lifelong").is_none());
    Ok(())
}

#[tokio::test]
async fn a_mapped_hold_keeps_its_span_and_records_once() -> TestResult {
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    let writes = Mutex::new(());
    let turn = lock("index.write").acquire(writes.lock()).await;
    let mapped = turn.map(|guard| (guard, 7_u8));
    assert_eq!(mapped.1, 7);
    assert!(
        writes.try_lock().is_err(),
        "the mapped value keeps the guard"
    );
    let holder = with_table(|table| table.listing(Duration::MAX).in_flight).ok_or("a table")?;
    drop(mapped);
    assert!(writes.try_lock().is_ok());
    let metrics = recorder.metrics();
    drop(recorder);

    assert_eq!(holder, 1, "one hold in flight across the map");
    assert_eq!(closed(&drain.queued_records(), "lock.held").len(), 1);
    let labels = [("lock.name", "index.write"), ("lock.mode", "exclusive")];
    assert_eq!(count(&metrics, "lock.held.duration", &labels), 1);
    Ok(())
}

/// A guard that reports, as it drops, whether its hold was still in the table.
struct Reporting(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Drop for Reporting {
    fn drop(&mut self) {
        let in_flight =
            with_table(|table| table.listing(Duration::MAX).in_flight).unwrap_or(u64::MAX);
        self.0.store(in_flight, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn a_hold_releases_its_guard_before_it_closes_its_record() -> TestResult {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let (recorder, _drain) = ScopedRecorder::builder().install()?;
    let seen = Arc::new(AtomicU64::new(u64::MAX));
    let held = lock("index.write")
        .try_acquire(|| Ok::<_, ()>(Reporting(Arc::clone(&seen))))
        .map_err(|()| "the attempt succeeds")?;
    drop(held);
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the hold was open as the guard dropped"
    );
    let mapped = lock("index.write")
        .try_acquire(|| Ok::<_, ()>(()))
        .map_err(|()| "the attempt succeeds")?
        .map(|()| Reporting(Arc::clone(&seen)));
    seen.store(u64::MAX, Ordering::SeqCst);
    drop(mapped);
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "a mapped value drops first too"
    );
    let after = with_table(|table| table.listing(Duration::MAX).in_flight).ok_or("a table")?;
    assert_eq!(after, 0, "the hold closed after the guard");
    drop(recorder);
    Ok(())
}
