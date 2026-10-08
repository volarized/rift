use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt as _;

use super::{
    LOG_SETTLE_TIMEOUT, LOG_WORKSPACE_QUEUE_RECORDS, LOG_WRITE_RETRY_INTERVAL, LaneProgress,
    LogRoutes, LogSettlement, RunningLogDrain, caused_by, workspace_of, write_retained,
};
use crate::{LOG_QUEUE_RECORDS, LogDrain, LogQuery, LogRecord, LogStore, log_capture};

/// A stop's deadline in these cases: the foreground server's four seconds.
const STOP_DEADLINE: Duration = Duration::from_secs(4);
/// Failure bound on one wait for the writer thread's close; never a way to order two
/// events, and the bound the store's own cases use. On windows-2025 under the workspace
/// suite, the close's `PRAGMA wal_checkpoint(TRUNCATE)` was measured past 2 s, all of it
/// in the file syncs the checkpoint runs, and one WAL switch at 4 s; the bound sits above
/// twice the longest of these.
const THREAD_WAIT_MAX: Duration = Duration::from_secs(10);

#[tokio::test(start_paused = true)]
async fn accepted_flush_interval_drives_batch_collection_and_reads_can_flush_early() {
    let interval = Duration::from_millis(17);
    let (sink, drain) = crate::capture::log_capture_with(super::LogDeliveryOptions {
        flush_interval: interval,
        ..super::LogDeliveryOptions::default()
    });
    let cancellation = CancellationToken::new();
    let started = tokio::time::Instant::now();
    assert!(matches!(
        drain.wait_for_flush(&cancellation).await,
        super::FlushReady::Due
    ));
    assert_eq!(started.elapsed(), interval);
    sink.settlement.flush.notify_one();
    let started = tokio::time::Instant::now();
    assert!(matches!(
        drain.wait_for_flush(&cancellation).await,
        super::FlushReady::Due
    ));
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "a read requests an immediate flush"
    );
    cancellation.cancel();
    assert!(matches!(
        drain.wait_for_flush(&cancellation).await,
        super::FlushReady::Cancelled
    ));
}

#[tokio::test(start_paused = true)]
async fn accepted_settlement_timeout_bounds_the_dispatchers_unwritten_record() {
    let timeout = Duration::from_millis(23);
    let (sink, _drain) = crate::capture::log_capture_with(super::LogDeliveryOptions {
        settle_timeout: timeout,
        ..super::LogDeliveryOptions::default()
    });
    sink.settlement.draining.store(true, Ordering::SeqCst);
    sink.send(record("unwritten"));
    let _subscriber = tracing::subscriber::set_default(crate::capture::registry().with(sink));
    let started = tokio::time::Instant::now();
    super::settle_for_read("/workspace").await;
    assert_eq!(started.elapsed(), timeout);
}

#[tokio::test(start_paused = true)]
async fn accepted_retry_interval_keeps_refused_records_until_success() {
    let interval = Duration::from_millis(31);
    let attempts = std::cell::Cell::new(0_u32);
    let started = tokio::time::Instant::now();
    super::write_retained_with(1, interval, || {
        let attempt = attempts.get() + 1;
        attempts.set(attempt);
        async move {
            if attempt < 3 {
                Err(rift_error::errors::tracing::log_store_failed()
                    .operation("append")
                    .path(std::path::Path::new(".rift/metrics"))
                    .detail(std::io::Error::other("database is locked"))
                    .error())
            } else {
                Ok(1)
            }
        }
    })
    .await;
    assert_eq!(attempts.get(), 3);
    assert_eq!(started.elapsed(), interval * 2);
}

/// One lane with `accepted` sequences stamped, the drain written through
/// `written_through`, and a drain that is running or is not.
fn settlement(accepted: u64, written_through: u64, draining: bool) -> LogSettlement {
    LogSettlement {
        accepted: AtomicU64::new(accepted),
        progress: tokio::sync::watch::Sender::new(LaneProgress {
            written_through,
            finished: written_through,
        }),
        flush: tokio::sync::Notify::new(),
        draining: AtomicBool::new(draining),
        routes: std::sync::OnceLock::new(),
        options: super::LogDeliveryOptions::default(),
    }
}

/// One read's wait on `lane`, under the bound every `rift://logs` read keeps.
async fn settle_for_read(lane: &LogSettlement) {
    lane.settle_by(tokio::time::Instant::now() + LOG_SETTLE_TIMEOUT)
        .await;
}

/// Drains what the queue currently holds, without a store.
fn queued(drain: &mut LogDrain) -> Vec<LogRecord> {
    std::iter::from_fn(|| drain.try_recv_record().ok()).collect()
}

fn record(message: &str) -> LogRecord {
    LogRecord::new(
        1,
        "info",
        "rift_tracing::drain",
        "logs",
        "logs.test",
        message,
        "{}",
    )
}

async fn store() -> (tempfile::TempDir, Arc<LogStore>) {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let store = LogStore::open(&directory.path().join("metrics"), None)
        .await
        .expect("the metrics database opens");
    (directory, Arc::new(store))
}

/// How many records `store` holds, read on a connection of its own.
fn count(store: &LogStore) -> u64 {
    store
        .reader()
        .connect()
        .and_then(|reads| reads.count())
        .expect("the count reads")
}

/// A process that installed no drain waits for nothing. Every command but the
/// foreground server records through no lane, and a log read there must not pay the
/// bound.
#[tokio::test(start_paused = true)]
async fn a_read_waits_for_a_drain_that_is_not_running() {
    let started = tokio::time::Instant::now();
    settle_for_read(&settlement(4, 0, false)).await;
    assert_eq!(
        tokio::time::Instant::now(),
        started,
        "a read with no drain behind it waits for nothing"
    );
}

/// A settled queue costs a read nothing.
#[tokio::test(start_paused = true)]
async fn a_read_over_a_settled_queue_waits_for_nothing() {
    let started = tokio::time::Instant::now();
    settle_for_read(&settlement(4, 4, true)).await;
    assert_eq!(tokio::time::Instant::now(), started);
}

/// A drain that stops settling still lets the read through, at the bound. A log read
/// never hangs because the log lane is stuck.
#[tokio::test(start_paused = true)]
async fn a_read_past_the_settle_bound_still_answers() {
    let started = tokio::time::Instant::now();
    settle_for_read(&settlement(4, 1, true)).await;
    assert_eq!(
        tokio::time::Instant::now() - started,
        LOG_SETTLE_TIMEOUT,
        "the read answers at the bound"
    );
}

/// The log write emits no record. Under a capture filter that admits every target at
/// `trace`, appends and a close on the metrics database leave the queue holding only the
/// record the test emitted: no write of a batch produces the records of the next one.
#[tokio::test]
async fn a_log_write_emits_no_record() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry()
        .with(sink.with_filter(tracing_subscriber::EnvFilter::new("trace")));
    tracing::subscriber::set_global_default(subscriber)
        .expect("this case owns the process's subscriber");
    let (_directory, store) = store().await;

    tracing::info!(component = "logs", "emitted by the test");
    for index in 0..4 {
        store
            .append([record(&format!("batch {index}"))], 10_000)
            .await
            .expect("the batch lands");
    }
    store
        .close(tokio::time::Instant::now() + THREAD_WAIT_MAX)
        .await
        .expect("the store closes");

    let records = queued(&mut drain);
    assert_eq!(
        records.iter().map(LogRecord::message).collect::<Vec<_>>(),
        ["emitted by the test"],
        "the queue holds only what the test emitted"
    );
}

/// The lane finishes with exactly the records it took. The drain mints a drop notice of
/// its own, and counting that among the records finished with pushes the total past what
/// the sink stamped, which would release a read waiting on a record still queued.
#[tokio::test]
async fn the_lane_finishes_with_exactly_the_records_it_took() {
    let (_directory, store) = store().await;
    let (sink, mut drain) = log_capture();
    for index in 0..=LOG_QUEUE_RECORDS {
        sink.send(record(&format!("record {index}")));
    }
    assert_eq!(
        sink.dropped(),
        1,
        "the queue holds one record less than sent"
    );

    // A short first turn, so the batch stays under the size at which the drain defers
    // its drop notice, and that turn writes the notice.
    let mut batch = Vec::new();
    for _ in 0..8 {
        batch.push(drain.receiver.try_recv().expect("the queue holds records"));
    }
    drain.write_turn(&store, &mut batch, 10_000).await;
    assert_eq!(
        count(&store),
        9,
        "the first turn writes its records and one drop notice"
    );

    while let Ok(queued) = drain.receiver.try_recv() {
        batch.push(queued);
    }
    drain.write_turn(&store, &mut batch, 10_000).await;

    let finished = sink.settlement.progress.borrow().finished;
    let accepted = sink.settlement.accepted.load(Ordering::SeqCst);
    assert_eq!(
        finished, accepted,
        "the lane took {accepted} records and finished with {finished}"
    );
}

/// A read whose own record the full queue dropped still answers. The sequence it waits
/// on never reaches the drain, so the lane's finished count is what releases it.
#[tokio::test(start_paused = true)]
async fn a_dropped_record_does_not_hold_a_read() {
    let (sink, _drain) = log_capture();
    sink.settlement.draining.store(true, Ordering::SeqCst);
    for index in 0..=LOG_QUEUE_RECORDS {
        sink.send(record(&format!("record {index}")));
    }
    assert_eq!(
        sink.dropped(),
        1,
        "the queue holds one record less than sent"
    );
    let settlement = Arc::clone(&sink.settlement);
    settlement.finish_written(LOG_QUEUE_RECORDS as u64, LOG_QUEUE_RECORDS as u64);
    let started = tokio::time::Instant::now();
    settle_for_read(&settlement).await;
    assert_eq!(
        tokio::time::Instant::now(),
        started,
        "the lane finished with every record it took, so the read does not wait"
    );
}

/// A read waits for the sequence it stamped, not for a count. A full queue drops the
/// newest record while older ones wait, so a lane that has finished with as many records
/// as the read stamped can still be holding the read's own record.
#[tokio::test(start_paused = true)]
async fn a_read_waits_for_its_own_sequence_not_for_a_count() {
    let lane = settlement(0, 0, true);
    lane.accepted.store(10, Ordering::SeqCst);
    lane.progress.send_modify(|progress| {
        progress.written_through = 4;
        progress.finished = 9;
    });
    let started = tokio::time::Instant::now();
    settle_for_read(&lane).await;
    assert_eq!(
        tokio::time::Instant::now() - started,
        LOG_SETTLE_TIMEOUT,
        "nine records finished with does not mean sequence ten was written"
    );
}

/// A read waits for the lane its thread's dispatcher records into, found through the
/// filtered, optional layer a serving process installs it as. A lane built later in the
/// same process is not that lane, and a thread whose dispatcher holds no sink waits for
/// nothing.
#[tokio::test(start_paused = true)]
async fn a_read_waits_for_the_lane_its_dispatcher_records_into() {
    let (sink, _drain) = log_capture();
    sink.settlement.draining.store(true, Ordering::SeqCst);
    sink.send(record("the beacon engine did not start"));
    let (_later_sink, _later_drain) = log_capture();

    let started = tokio::time::Instant::now();
    {
        let _without_sink = tracing::subscriber::set_default(crate::capture::registry());
        super::settle_for_read("/workspace").await;
    }
    assert_eq!(
        tokio::time::Instant::now(),
        started,
        "a thread whose dispatcher holds no sink waits for nothing"
    );

    let subscriber = crate::capture::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::sink)
                .with_filter(tracing_subscriber::EnvFilter::new("info")),
        )
        .with(Some(
            sink.with_filter(tracing_subscriber::EnvFilter::new("info")),
        ));
    let _with_sink = tracing::subscriber::set_default(subscriber);
    super::settle_for_read("/workspace").await;
    assert_eq!(
        tokio::time::Instant::now() - started,
        LOG_SETTLE_TIMEOUT,
        "the read waits for its own lane's unwritten record, not the later lane"
    );
}

/// The lane counts what it accepted and never finished, and drops no batch carried yet.
#[tokio::test]
async fn a_lane_counts_its_unwritten_records() {
    let (sink, drain) = log_capture();
    let lane = drain.lane();
    assert_eq!(
        lane.unwritten(),
        0,
        "an empty lane leaves nothing unwritten"
    );
    let subscriber = crate::capture::registry().with(sink.clone());
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(component = "logs", "first");
        tracing::info!(component = "logs", "second");
    });
    assert_eq!(lane.unwritten(), 2, "queued records are unwritten");
    drain.dropped.fetch_add(3, Ordering::Relaxed);
    assert_eq!(lane.unwritten(), 5, "drops no batch carried count too");

    let (_directory, store) = store().await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    drain.run(Arc::clone(&store), 100, cancellation).await;

    assert_eq!(
        lane.unwritten(),
        0,
        "a flushed lane leaves nothing unwritten"
    );
    assert_eq!(
        count(&store),
        3,
        "the two records and the drop notice landed"
    );
}

/// A batch the store refuses stays with the drain until the store accepts it, past the
/// five attempts after which the drain once dropped it. Each refusal waits the retry
/// interval on the drain's own timer, and the records reach the attempt that lands.
#[tokio::test(start_paused = true)]
async fn a_refused_batch_is_kept_until_the_store_accepts_it() {
    const REFUSALS: u32 = 7;
    let records = [record("one"), record("two")];
    let attempts = std::cell::Cell::new(0_u32);
    let started = tokio::time::Instant::now();

    write_retained(records.len(), || {
        attempts.set(attempts.get() + 1);
        let attempt = attempts.get();
        let held = records.len();
        async move {
            assert_eq!(held, 2, "attempt {attempt} carries the whole batch");
            if attempt <= REFUSALS {
                Err(rift_error::errors::tracing::log_store_failed()
                    .operation("append")
                    .path(std::path::Path::new(".rift/metrics"))
                    .detail(std::io::Error::other("database is locked"))
                    .error())
            } else {
                Ok(0)
            }
        }
    })
    .await;

    assert_eq!(
        attempts.get(),
        REFUSALS + 1,
        "the batch landed on the attempt after the refusals"
    );
    assert_eq!(
        tokio::time::Instant::now() - started,
        LOG_WRITE_RETRY_INTERVAL * REFUSALS,
        "each refusal waits one retry interval"
    );
}

/// A drain whose flush meets another connection's write lock on the metrics database
/// keeps its batch and outlasts its bound; once aborted, its lane reports every record
/// it accepted.
#[tokio::test(start_paused = true)]
async fn an_aborted_drain_behind_a_held_metrics_lock_leaves_its_records_unwritten() {
    const ACCEPTED: u64 = 4;
    let (directory, store) = store().await;
    let holder = rusqlite::Connection::open(directory.path().join("metrics"))
        .expect("the holding connection opens");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the holding connection takes the write lock");
    let (sink, drain) = log_capture();
    let lane = drain.lane();
    tracing::subscriber::with_default(crate::capture::registry().with(sink), || {
        for record in 0..ACCEPTED {
            tracing::info!(component = "logs", record, "behind the held lock");
        }
    });
    let cancellation = CancellationToken::new();
    let mut task = tokio::spawn(drain.run(Arc::clone(&store), 100, cancellation.clone()));
    cancellation.cancel();

    let bound = tokio::time::Instant::now() + Duration::from_millis(500);
    let joined = tokio::time::timeout_at(bound, &mut task).await;
    assert!(
        joined.is_err(),
        "the flush waits on the held lock past its bound"
    );
    task.abort();
    let _ = task.await;

    assert_eq!(lane.unwritten(), ACCEPTED);
    drop(holder);
}

/// A stop whose drain meets another connection's write lock on the metrics database past
/// its deadline warns "log drain outlasted the stop deadline", and the warning's
/// `unwritten` field counts every record the lane accepted and never wrote.
#[tokio::test(start_paused = true)]
async fn a_stop_behind_a_held_metrics_lock_warns_with_the_unwritten_count() {
    const ACCEPTED: u64 = 4;
    let (directory, store) = store().await;
    let holder = rusqlite::Connection::open(directory.path().join("metrics"))
        .expect("the holding connection opens");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the holding connection takes the write lock");
    let (sink, drain) = log_capture();
    tracing::subscriber::with_default(crate::capture::registry().with(sink), || {
        for record in 0..ACCEPTED {
            tracing::info!(component = "logs", record, "behind the held lock");
        }
    });
    let running = RunningLogDrain::spawn(drain, Arc::clone(&store), 100);
    let (recorder, mut records) = crate::ScopedRecorder::builder()
        .install()
        .expect("the default filter parses");

    let unwritten = running
        .stop(tokio::time::Instant::now() + Duration::from_millis(500))
        .await;
    let snapshot = recorder.metrics();
    drop(recorder);

    assert_eq!(unwritten, Some(ACCEPTED));
    let dropped = snapshot
        .find("log.queue.dropped", &[("error.type", "unwritten")])
        .expect("the stop counts the records it never wrote");
    assert_eq!(dropped.unit(), "{record}");
    #[expect(clippy::cast_precision_loss, reason = "four records convert exactly")]
    let expected = ACCEPTED as f64;
    assert_eq!(dropped.value(), &crate::SeriesValue::Sum(expected));
    let warning = records
        .queued_records()
        .into_iter()
        .find(|record| record.message() == "log drain outlasted the stop deadline")
        .expect("the stop warns that the drain outlasted its deadline");
    assert_eq!(warning.level(), "warn");
    let fields: serde_json::Value =
        serde_json::from_str(warning.fields()).expect("the warning's fields are JSON");
    assert_eq!(fields["unwritten"], ACCEPTED.to_string(), "{fields}");
    drop(holder);
}

#[tokio::test]
async fn cancellation_drains_every_buffered_record() {
    let (_directory, store) = store().await;
    let (sink, drain) = log_capture();
    for message in ["one", "two", "three"] {
        sink.send(record(message));
    }
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    drain.run(Arc::clone(&store), 10_000, cancellation).await;

    assert_eq!(count(&store), 3);
}

#[tokio::test]
async fn a_full_batch_defers_the_drop_record_without_oversizing_the_write() {
    let (_directory, store) = store().await;
    let (sink, drain) = log_capture();
    for index in 0..(LOG_QUEUE_RECORDS + 8) {
        sink.send(record(&format!("record {index}")));
    }
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    drain.run(Arc::clone(&store), 10_000, cancellation).await;

    assert_eq!(count(&store), (LOG_QUEUE_RECORDS + 1) as u64);
    let latest = store
        .reader()
        .connect()
        .and_then(|reads| reads.recent(&LogQuery::newest(1)))
        .expect("the latest record reads");
    assert_eq!(
        latest[0].record().message(),
        "the log queue was full and dropped records"
    );
    assert_eq!(latest[0].record().fields(), "{\"dropped\":8}");
}

#[test]
fn a_failure_renders_its_causes_after_its_own_text() {
    let refused = rift_error::errors::mcp::election_storage_failed()
        .operation("publish")
        .path(std::path::Path::new(".rift/server.json"))
        .source(std::io::Error::other("disk full"))
        .error();
    assert_eq!(caused_by(&refused), ": disk full");
    assert_eq!(caused_by(&std::io::Error::other("disk full")), "");
}

/// A running drain over `task`, on a lane of its own.
fn running_drain(task: tokio::task::JoinHandle<()>) -> RunningLogDrain {
    RunningLogDrain {
        task,
        lane: log_capture().1.lane(),
        stop: CancellationToken::new(),
        route: None,
    }
}

#[tokio::test]
async fn a_failed_log_drain_is_joined() {
    let drain = tokio::spawn(async { panic!("injected log drain failure") });

    let unwritten = running_drain(drain)
        .stop(tokio::time::Instant::now() + STOP_DEADLINE)
        .await;

    assert_eq!(unwritten, None, "a joined drain leaves nothing to count");
}

/// A drain aborted at its bound reports every record the lane accepted and never wrote.
#[tokio::test(start_paused = true)]
async fn an_aborted_log_drain_counts_the_records_it_never_wrote() {
    const ACCEPTED: u64 = 3;
    let (sink, drain) = log_capture();
    let lane = drain.lane();
    tracing::subscriber::with_default(crate::capture::registry().with(sink), || {
        for record in 0..ACCEPTED {
            tracing::info!(component = "test", record, "accepted and never written");
        }
    });
    // The task holds the drain's queue open and never writes it, as a drain does whose
    // store refuses every batch.
    let task = tokio::spawn(async move {
        let _held = drain;
        std::future::pending::<()>().await;
    });
    let running = RunningLogDrain {
        task,
        lane,
        stop: CancellationToken::new(),
        route: None,
    };

    let started = tokio::time::Instant::now();
    let unwritten = running.stop(started + STOP_DEADLINE).await;

    assert_eq!(unwritten, Some(ACCEPTED));
}

/// A stalled drain is aborted at its deadline and no later: the stop that holds it
/// keeps the rest of its own bound.
#[tokio::test(start_paused = true)]
async fn a_stalled_log_drain_is_aborted_at_its_deadline() {
    struct Stopped(Arc<AtomicBool>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let stopped = Arc::new(AtomicBool::new(false));
    let task_stopped = Arc::clone(&stopped);
    let drain = tokio::spawn(async move {
        let _stopped = Stopped(task_stopped);
        std::future::pending::<()>().await;
    });
    tokio::task::yield_now().await;

    let started = tokio::time::Instant::now();
    let unwritten = running_drain(drain).stop(started + STOP_DEADLINE).await;

    assert_eq!(unwritten, Some(0), "an empty lane leaves nothing unwritten");
    assert!(stopped.load(Ordering::Acquire));
    assert_eq!(
        started.elapsed(),
        STOP_DEADLINE,
        "the stop aborts the drain at its deadline"
    );
}

/// A record the full queue refuses adds one to `log.queue.dropped` with `error.type`
/// `queue_full`, beside the sink's own count.
#[test]
fn a_full_queue_counts_each_refused_record_in_log_queue_dropped() {
    let (recorder, _records) = crate::ScopedRecorder::builder()
        .install()
        .expect("the default filter parses");
    let (sink, _drain) = log_capture();
    for index in 0..=LOG_QUEUE_RECORDS {
        sink.send(record(&format!("record {index}")));
    }
    assert_eq!(sink.dropped(), 1, "the queue refused one record");

    let snapshot = recorder.metrics();
    let dropped = snapshot
        .find("log.queue.dropped", &[("error.type", "queue_full")])
        .expect("the refused record is counted");
    assert_eq!(dropped.unit(), "{record}");
    assert_eq!(dropped.labels(), [("error.type", "queue_full")]);
    assert_eq!(dropped.value(), &crate::SeriesValue::Sum(1.0));
}

/// The messages `store` holds, oldest first, read on a connection of its own.
fn stored_messages(store: &LogStore) -> Vec<String> {
    let mut records = store
        .reader()
        .connect()
        .and_then(|reads| reads.recent(&LogQuery::newest(1_000)))
        .expect("the records read");
    records.reverse();
    records
        .iter()
        .map(|stored| stored.record().message().to_owned())
        .collect()
}

/// One record whose fields are `fields`.
fn record_with_fields(message: &str, fields: &str) -> LogRecord {
    LogRecord::new(
        1,
        "info",
        "rift_tracing::drain",
        "logs",
        "logs.test",
        message,
        fields,
    )
}

/// A record names its workspace in its own field, in the span it was emitted in, or in
/// the outermost span around it, in that order; fields that name none, or do not parse,
/// name no workspace.
#[test]
fn a_record_names_the_workspace_its_fields_or_spans_carry() {
    let cases = [
        (r#"{"workspace":"/a"}"#, Some("/a")),
        (
            r#"{"root_span":{"name":"mcp.request","fields":{"workspace":"/b"}}}"#,
            Some("/b"),
        ),
        (
            r#"{"root_span":{"name":"mcp.request","fields":{"workspace":"/b"}},"nearest_span":{"name":"index.read","fields":{"workspace":"/c"}}}"#,
            Some("/c"),
        ),
        (
            r#"{"workspace":"/a","root_span":{"name":"mcp.request","fields":{"workspace":"/b"}}}"#,
            Some("/a"),
        ),
        (r#"{"root":"/a"}"#, None),
        (r#"{"workspace":7}"#, None),
        (r#"{"workspace":"/a""#, None),
        ("{}", None),
    ];
    for (fields, expected) in cases {
        assert_eq!(
            workspace_of(&record_with_fields("routed", fields)).as_deref(),
            expected,
            "{fields}"
        );
    }
}

/// Under a routing drain, each workspace's store holds the records that name it, from a
/// span around them or from their own field, and no other workspace's; a read in a
/// workspace waits for that workspace's consumer. A stopped consumer leaves the routes.
#[tokio::test]
async fn a_routing_drain_writes_each_record_into_its_own_workspace_store() {
    let (sink, drain) = log_capture();
    let subscriber = crate::capture::registry()
        .with(sink.with_filter(tracing_subscriber::EnvFilter::new("info")));
    let _default = tracing::subscriber::set_default(subscriber);
    let router = RunningLogDrain::spawn_routed(drain, 1_000);
    let (_first_directory, first_store) = store().await;
    let (_second_directory, second_store) = store().await;
    let first = RunningLogDrain::for_workspace("/first", Arc::clone(&first_store))
        .expect("a routing drain starts a consumer");
    let second = RunningLogDrain::for_workspace("/second", Arc::clone(&second_store))
        .expect("a routing drain starts a consumer");

    tracing::info_span!("mcp.request", component = "mcp", workspace = "/first").in_scope(|| {
        tracing::info_span!("index.read", component = "index")
            .in_scope(|| tracing::info!("read inside the first workspace's request"));
    });
    tracing::info!(workspace = "/second", "named by its own field");
    tracing::info!("named by nothing");
    super::settle_for_read("/first").await;
    super::settle_for_read("/second").await;

    let first_messages = stored_messages(&first_store);
    assert!(
        first_messages.contains(&"read inside the first workspace's request".to_owned()),
        "{first_messages:?}"
    );
    assert!(
        first_messages.contains(&"mcp.request".to_owned()),
        "the request's own close record names the workspace: {first_messages:?}"
    );
    assert_eq!(
        stored_messages(&second_store),
        ["named by its own field"],
        "the second store holds its own record alone"
    );

    let deadline = tokio::time::Instant::now() + STOP_DEADLINE;
    assert_eq!(first.stop(deadline).await, None);
    tracing::info!(workspace = "/first", "after the first consumer stopped");
    super::settle_for_read("/first").await;
    assert!(
        !stored_messages(&first_store).contains(&"after the first consumer stopped".to_owned()),
        "a stopped consumer receives nothing"
    );
    assert_eq!(second.stop(deadline).await, None);
    assert_eq!(router.stop(deadline).await, None);
}

/// A record that names no workspace, or a workspace with no consumer, reaches no store,
/// and the routing lane still finishes with it, so no read waits on it.
#[tokio::test]
async fn a_record_with_no_workspace_reaches_no_store() {
    let (sink, drain) = log_capture();
    let settlement = Arc::clone(&sink.settlement);
    let subscriber = crate::capture::registry()
        .with(sink.with_filter(tracing_subscriber::EnvFilter::new("info")));
    let _default = tracing::subscriber::set_default(subscriber);
    let router = RunningLogDrain::spawn_routed(drain, 1_000);
    let (_directory, store) = store().await;
    let consumer = RunningLogDrain::for_workspace("/served", Arc::clone(&store))
        .expect("a routing drain starts a consumer");

    tracing::info!("named by nothing");
    tracing::info!(
        workspace = "/unserved",
        "named by a workspace with no consumer"
    );
    super::settle_for_read("/served").await;

    let deadline = tokio::time::Instant::now() + STOP_DEADLINE;
    assert_eq!(consumer.stop(deadline).await, None);
    assert_eq!(router.stop(deadline).await, None);
    assert_eq!(count(&store), 0, "neither record reached the store");
    assert_eq!(
        settlement.progress.borrow().finished,
        settlement.accepted.load(Ordering::SeqCst),
        "the routing lane finished with every record it took"
    );
}

/// A full consumer queue drops the routed record, counts each drop in
/// `log.queue.dropped` with `error.type` `queue_full`, and its store holds one notice for
/// all of them, not one record per drop.
#[tokio::test]
async fn a_full_workspace_queue_counts_each_drop_in_log_queue_dropped() {
    const OVERFLOW: u32 = 5;
    let (recorder, _records) = crate::ScopedRecorder::builder()
        .install()
        .expect("the default filter parses");
    let routes = Arc::new(LogRoutes::new(10_000));
    let upstream = Arc::new(LogSettlement::default());
    let (_directory, store) = store().await;
    let consumer = routes
        .admit(&upstream, "/served", Arc::clone(&store))
        .expect("the first consumer is admitted");

    // The consumer task cannot run before this loop yields, so the queue fills.
    for index in 0..LOG_WORKSPACE_QUEUE_RECORDS + OVERFLOW as usize {
        routes.route(record_with_fields(
            &format!("record {index}"),
            r#"{"workspace":"/served"}"#,
        ));
    }
    assert_eq!(
        consumer
            .stop(tokio::time::Instant::now() + STOP_DEADLINE)
            .await,
        None
    );

    let snapshot = recorder.metrics();
    let dropped = snapshot
        .find("log.queue.dropped", &[("error.type", "queue_full")])
        .expect("the refused records are counted");
    assert_eq!(
        dropped.value(),
        &crate::SeriesValue::Sum(f64::from(OVERFLOW))
    );
    let messages = stored_messages(&store);
    assert_eq!(messages.len(), LOG_WORKSPACE_QUEUE_RECORDS + 1);
    assert_eq!(
        messages
            .iter()
            .filter(|message| *message == "the log queue was full and dropped records")
            .count(),
        1,
        "one notice carries every drop"
    );
}

/// A routing drain keeps at most `WORKSPACE_CONSUMERS_MAX` consumers: one more workspace
/// starts none. Replacing a routed workspace also needs a free retained budget.
#[tokio::test]
async fn routes_past_the_consumer_bound_start_no_consumer() {
    let routes = Arc::new(LogRoutes::new(10_000));
    let upstream = Arc::new(LogSettlement::default());
    let (_directory, store) = store().await;
    let mut consumers = (0..RunningLogDrain::WORKSPACE_CONSUMERS_MAX)
        .map(|index| {
            routes
                .admit(&upstream, &format!("/w{index}"), Arc::clone(&store))
                .expect("a consumer under the bound is admitted")
        })
        .collect::<Vec<_>>();

    assert!(
        routes
            .admit(&upstream, "/one-more", Arc::clone(&store))
            .is_none(),
        "the bound refuses one more workspace"
    );
    let current = routes
        .settlement_of("/w0")
        .expect("the route remains admitted");
    assert!(routes.admit(&upstream, "/w0", Arc::clone(&store)).is_none());
    assert!(Arc::ptr_eq(
        &current,
        &routes
            .settlement_of("/w0")
            .expect("refusal preserves the route")
    ));
    let deadline = tokio::time::Instant::now() + STOP_DEADLINE;
    assert_eq!(consumers.remove(0).stop(deadline).await, None);
    let replaced = routes
        .admit(&upstream, "/w0", Arc::clone(&store))
        .expect("a released budget admits the workspace again");
    consumers.push(replaced);

    for consumer in consumers {
        assert_eq!(consumer.stop(deadline).await, None);
    }
    assert!(
        routes.consumers().routes.is_empty(),
        "every stop left the routes"
    );
}

/// Replaced consumers still count while their sending ends keep the tasks alive.
#[tokio::test]
async fn repeated_workspace_replacements_keep_the_retained_consumer_bound() {
    let routes = Arc::new(LogRoutes::new(10_000));
    let upstream = Arc::new(LogSettlement::default());
    let (_directory, store) = store().await;
    let mut consumers = Vec::new();
    let mut retained = Vec::new();
    for _ in 0..RunningLogDrain::WORKSPACE_CONSUMERS_MAX {
        consumers.push(
            routes
                .admit(&upstream, "/same", Arc::clone(&store))
                .expect("a retained consumer under the bound is admitted"),
        );
        retained.push(
            routes
                .consumers()
                .routes
                .get("/same")
                .expect("the admitted route is present")
                .clone(),
        );
    }
    assert_eq!(routes.consumers().routes.len(), 1);
    let current = routes
        .settlement_of("/same")
        .expect("the current route is present");
    assert!(
        routes
            .admit(&upstream, "/same", Arc::clone(&store))
            .is_none()
    );
    assert!(Arc::ptr_eq(
        &current,
        &routes
            .settlement_of("/same")
            .expect("refusal preserves the route")
    ));
    let deadline = tokio::time::Instant::now() + STOP_DEADLINE;
    drop(retained.remove(0));
    assert_eq!(consumers.remove(0).stop(deadline).await, None);
    consumers.push(
        routes
            .admit(&upstream, "/same", store)
            .expect("release admits a replacement"),
    );
    assert!(!Arc::ptr_eq(
        &current,
        &routes
            .settlement_of("/same")
            .expect("replacement changes the route")
    ));
    retained.clear();
    for consumer in consumers {
        assert_eq!(consumer.stop(deadline).await, None);
    }
    assert!(routes.consumers().routes.is_empty());
}

/// Queued records and writer batches keep retired budgets admitted until released.
#[tokio::test]
async fn queued_and_writer_batches_keep_retired_consumer_budgets_admitted() {
    let routes = Arc::new(LogRoutes::new(10_000));
    let upstream = Arc::new(LogSettlement::default());
    let (_directory, store) = store().await;
    let mut queues = Vec::new();
    for _ in 0..RunningLogDrain::WORKSPACE_CONSUMERS_MAX {
        let running = routes
            .admit(&upstream, "/same", Arc::clone(&store))
            .expect("a retained budget under the bound is admitted");
        let route = routes
            .consumers()
            .routes
            .get("/same")
            .expect("the current route is present")
            .clone();
        let value = record("retained");
        let bytes = route
            .budget
            .reserve(&value)
            .expect("one record fits the budget");
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender
            .try_send(super::QueuedRecord {
                sequence: 1,
                record: value,
                bytes: Some(bytes),
            })
            .expect("one record fits the queue");
        drop(sender);
        queues.push(receiver);
        running.task.abort();
        assert!(running.task.await.is_err());
    }
    let current = routes
        .settlement_of("/same")
        .expect("the current route is present");
    assert!(
        routes
            .admit(&upstream, "/same", Arc::clone(&store))
            .is_none()
    );
    assert!(Arc::ptr_eq(
        &current,
        &routes
            .settlement_of("/same")
            .expect("queued records preserve admission")
    ));
    let mut batches = queues
        .into_iter()
        .map(|mut receiver| {
            let item = receiver.try_recv().expect("the queue retains its record");
            Arc::new(crate::store::RetainedLogBatch {
                records: Arc::from([item.record]),
                _bytes: vec![item.bytes],
            })
        })
        .collect::<Vec<_>>();
    assert!(
        routes
            .admit(&upstream, "/same", Arc::clone(&store))
            .is_none()
    );
    assert!(Arc::ptr_eq(
        &current,
        &routes
            .settlement_of("/same")
            .expect("writer batches preserve admission")
    ));
    drop(batches.remove(0));
    let replacement = routes
        .admit(&upstream, "/same", store)
        .expect("a released writer batch admits a replacement");
    drop(batches);
    assert_eq!(
        replacement
            .stop(tokio::time::Instant::now() + STOP_DEADLINE)
            .await,
        None
    );
}

/// A dispatcher whose lane does not route starts no workspace consumer.
#[tokio::test]
async fn a_lane_that_does_not_route_starts_no_consumer() {
    let (sink, _drain) = log_capture();
    let _default = tracing::subscriber::set_default(crate::capture::registry().with(sink));
    let (_directory, store) = store().await;

    assert!(RunningLogDrain::for_workspace("/served", store).is_none());
}
