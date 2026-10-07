use std::sync::{Arc, Barrier};

use super::{LOG_SUBSCRIPTION_BYTES_MAX, LOG_SUBSCRIPTIONS_MAX, LogStream, record_bytes};
use crate::capture::log_capture_with;
use crate::drain::LogDeliveryOptions;
use crate::{LOG_FIELDS_BYTES_MAX, LOG_MESSAGE_BYTES_MAX, LogRecord, log_capture};

#[tokio::test]
async fn a_batch_read_stops_at_the_declared_record_bound() {
    let records = crate::LOG_BATCH_RECORDS_MAX + 1;
    let (sink, _persistence) = log_capture_with(LogDeliveryOptions {
        queue_records: records,
        ..LogDeliveryOptions::default()
    });
    let mut subscription = sink.logs.subscribe().expect("subscription fits");
    for _ in 0..records {
        sink.send(record("bounded batch"));
    }
    subscription.close();
    assert_eq!(
        subscription.recv_batch().await.len(),
        crate::LOG_BATCH_RECORDS_MAX
    );
    assert_eq!(subscription.recv_batch().await.len(), 1);
    assert!(subscription.recv_batch().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_receive_keeps_later_delivery_registered() {
    let (sink, _persistence) = log_capture();
    let mut subscription = sink.logs.subscribe().expect("subscription fits");
    let timed_out =
        tokio::time::timeout(std::time::Duration::from_millis(1), subscription.recv()).await;
    assert!(timed_out.is_err());
    sink.send(record("after cancellation"));
    assert_eq!(
        subscription
            .recv()
            .await
            .expect("later delivery remains queued")
            .record()
            .message(),
        "after cancellation"
    );
}

fn record(message: &str) -> LogRecord {
    LogRecord::new(
        1,
        "info",
        "rift_tracing",
        "logs",
        "logs.test",
        message,
        "{}",
    )
}

#[tokio::test]
async fn registration_receives_only_later_publications() {
    let (sink, mut persistence) = log_capture();
    sink.send(record("before"));
    let mut subscription = sink.logs.subscribe().expect("one subscription fits");
    sink.send(record("after"));
    let publication = subscription.recv().await.expect("later record arrives");
    assert_eq!(publication.sequence(), 2);
    assert_eq!(publication.record().message(), "after");
    assert_eq!(
        persistence
            .try_recv_record()
            .expect("first record")
            .message(),
        "before"
    );
    assert_eq!(
        persistence
            .try_recv_record()
            .expect("second record")
            .message(),
        "after"
    );
}

#[tokio::test]
async fn one_full_subscription_leaves_other_subscriptions_receiving() {
    let (sink, mut persistence) = log_capture_with(LogDeliveryOptions {
        queue_records: 1,
        ..LogDeliveryOptions::default()
    });
    let mut slow = sink.logs.subscribe().expect("slow subscription fits");
    let mut fast = sink.logs.subscribe().expect("fast subscription fits");
    sink.send(record("first"));
    let first = fast.recv().await.expect("first record");
    persistence
        .try_recv_record()
        .expect("first persisted record");
    sink.send(record("second"));
    let second = fast.recv().await.expect("second record");
    assert_eq!(first.sequence(), 1);
    assert_eq!(second.sequence(), 2);
    assert_eq!(slow.dropped(), 1);
    assert_eq!(fast.dropped(), 0);
    drop(slow.recv().await.expect("slow first record"));
    sink.send(record("third"));
    let third = slow.recv().await.expect("delivery resumes after loss");
    assert_eq!(third.sequence(), 3);
    assert_eq!(third.record().message(), "third");
}

#[test]
fn admission_counts_persistence_and_reuses_released_slots() {
    let (sink, _persistence) = log_capture();
    let mut subscriptions: Vec<_> = (1..LOG_SUBSCRIPTIONS_MAX)
        .map(|_| {
            sink.logs
                .subscribe()
                .expect("subscription fits its declared limit")
        })
        .collect();
    let error = sink
        .logs
        .subscribe()
        .expect_err("persistence spends the first subscription");
    assert_eq!(
        error.slug(),
        rift_error::errors::tracing::log_subscription_limit::SLUG
    );
    assert!(
        error
            .context()
            .any(|(key, value)| key == "maximum" && value == "16")
    );
    drop(subscriptions.pop());
    assert!(
        sink.logs.subscribe().is_ok(),
        "drop releases one admission slot"
    );
}

#[tokio::test]
async fn closure_drains_buffered_records_and_released_slot_keeps_its_new_owner() {
    let (sink, _persistence) = log_capture();
    let mut old = sink.logs.subscribe().expect("old subscription fits");
    sink.send(record("before closure"));
    old.close();
    let mut new = sink
        .logs
        .subscribe()
        .expect("closed subscription released its slot");
    drop(
        old.recv()
            .await
            .expect("buffered publication survives closure"),
    );
    assert!(old.recv().await.is_none());
    drop(old);
    sink.send(record("after closure"));
    let received = new
        .recv()
        .await
        .expect("old drop leaves replacement registered");
    assert_eq!(received.record().message(), "after closure");
}

#[tokio::test]
async fn stream_closure_keeps_buffered_publications_and_refuses_registration() {
    let (sink, _persistence) = log_capture();
    let mut subscription = sink.logs.subscribe().expect("subscription fits");
    sink.send(record("buffered"));
    sink.logs.close();
    assert_eq!(
        sink.logs
            .subscribe()
            .expect_err("a closed stream refuses subscription")
            .slug()
            .as_str(),
        "rift.tracing.log_stream_unavailable"
    );
    assert_eq!(subscription.recv_batch().await.len(), 1);
    assert!(subscription.recv().await.is_none());
    assert_eq!(
        LogStream::new(1, false)
            .subscribe()
            .expect_err("disabled capture refuses subscription")
            .slug()
            .as_str(),
        "rift.tracing.log_stream_unavailable"
    );
}

#[tokio::test]
async fn fan_out_shares_one_immutable_record() {
    let (sink, _persistence) = log_capture();
    let mut first = sink.logs.subscribe().expect("first subscription fits");
    let mut second = sink.logs.subscribe().expect("second subscription fits");
    sink.send(record("shared"));
    let first = first.recv().await.expect("first record");
    let second = second.recv().await.expect("second record");
    assert!(
        std::ptr::eq(first.record(), second.record()),
        "fan-out shares the payload allocation"
    );
}

#[tokio::test]
async fn retained_batch_spends_the_same_byte_capacity_as_the_queue() {
    let (sink, mut persistence) = log_capture();
    let mut subscription = sink.logs.subscribe().expect("subscription fits");
    let large = LogRecord::new(
        1,
        "info",
        "rift_tracing",
        "logs",
        "logs.test",
        &"m".repeat(LOG_MESSAGE_BYTES_MAX),
        &"f".repeat(LOG_FIELDS_BYTES_MAX),
    );
    let records_within_bytes = LOG_SUBSCRIPTION_BYTES_MAX / record_bytes(&large);
    for _ in 0..records_within_bytes {
        sink.send(large.clone());
        persistence
            .try_recv_record()
            .expect("persistence is drained independently");
    }
    assert_eq!(subscription.dropped(), 0, "exactly the byte capacity fits");
    let held = subscription.recv_batch().await;
    assert!(!held.is_empty());
    sink.send(large.clone());
    assert_eq!(
        subscription.dropped(),
        1,
        "retained batch keeps its byte capacity"
    );
    drop(held);
    sink.send(large);
    assert_eq!(
        subscription.dropped(),
        1,
        "released bytes admit the next record"
    );
}

#[test]
fn concurrent_publishers_keep_one_order_in_every_subscription_and_persistence() {
    const PUBLISHERS: usize = 4;
    const RECORDS_PER_PUBLISHER: usize = 32;
    let (sink, mut persistence) = log_capture();
    let mut first = sink.logs.subscribe().expect("first subscription fits");
    let mut second = sink.logs.subscribe().expect("second subscription fits");
    let gate = Arc::new(Barrier::new(PUBLISHERS + 1));
    let publication_lock = sink.logs.publications();
    let workers: Vec<_> = (0..PUBLISHERS)
        .map(|publisher| {
            let sink = sink.clone();
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.wait();
                for index in 0..RECORDS_PER_PUBLISHER {
                    sink.send(record(&format!("{publisher}:{index}")));
                }
            })
        })
        .collect();
    gate.wait();
    drop(publication_lock);
    for worker in workers {
        worker.join().expect("publisher finishes");
    }
    for sequence in 1..=(PUBLISHERS * RECORDS_PER_PUBLISHER) as u64 {
        let first = first
            .receiver
            .try_recv()
            .expect("first subscription receives every record");
        let second = second
            .receiver
            .try_recv()
            .expect("second subscription receives every record");
        let persisted = persistence
            .try_recv_record()
            .expect("persistence receives every record");
        assert_eq!(first.sequence(), sequence);
        assert_eq!(second.sequence(), sequence);
        assert_eq!(first.record(), second.record());
        assert_eq!(first.record(), &persisted);
    }
}
