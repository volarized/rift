//! Public runtime log subscription delivery.

use std::time::Duration;

use rift_tracing::TracingRuntime;

#[tokio::test]
async fn installed_runtime_delivers_independent_publications_and_closes_after_shutdown()
-> Result<(), Box<dyn std::error::Error>> {
    let (runtime, persistence) = TracingRuntime::builder()
        .capture("info")
        .queue_records(2)
        .install()?;
    let mut persistence = persistence.ok_or("capture creates the persistence subscription")?;
    let mut first = runtime.logs().subscribe()?;
    let mut second = runtime.logs().subscribe()?;
    rift_tracing::info!(component = "logs", "subscription publication");
    let first = tokio::time::timeout(Duration::from_secs(2), first.recv())
        .await?
        .ok_or("first subscription receives the publication")?;
    let second_record = tokio::time::timeout(Duration::from_secs(2), second.recv())
        .await?
        .ok_or("second subscription receives the publication")?;
    assert_eq!(first.sequence(), second_record.sequence());
    assert!(std::ptr::eq(first.record(), second_record.record()));
    assert_eq!(first.record().message(), "subscription publication");
    assert_eq!(persistence.try_recv_record()?, *first.record());
    runtime.shutdown().await?;
    let final_records = second.recv_batch().await;
    assert!(
        final_records.len() <= 2,
        "shutdown records retain the queue's record bound"
    );
    assert!(
        second.recv().await.is_none(),
        "shutdown ends stream delivery"
    );
    Ok(())
}
