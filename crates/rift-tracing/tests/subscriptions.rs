//! Public runtime log subscription delivery.

use std::time::Duration;

use rift_tracing::TracingRuntime;

#[tokio::test]
async fn installed_runtime_delivers_independent_publications_and_closes_after_shutdown()
-> Result<(), Box<dyn std::error::Error>> {
    let (runtime, persistence) = TracingRuntime::builder()
        .capture(&format!("{}=info", module_path!()))
        .queue_records(2)
        .install()?;
    let mut persistence = persistence.ok_or("capture creates the persistence subscription")?;
    let mut first = runtime.logs().subscribe()?;
    let mut second = runtime.logs().subscribe()?;
    let messages = [
        "subscription publication",
        "second subscription publication",
    ];
    for message in messages {
        rift_tracing::info!(component = "logs", "{message}");
    }
    let mut previous = 0;
    for message in messages {
        let first_record = tokio::time::timeout(Duration::from_secs(2), first.recv())
            .await?
            .ok_or("first subscription receives the publication")?;
        let second_record = tokio::time::timeout(Duration::from_secs(2), second.recv())
            .await?
            .ok_or("second subscription receives the publication")?;
        assert!(first_record.sequence() > previous);
        assert_eq!(first_record.sequence(), second_record.sequence());
        assert!(std::ptr::eq(first_record.record(), second_record.record()));
        assert_eq!(first_record.record().message(), message);
        assert_eq!(persistence.try_recv_record()?, *first_record.record());
        previous = first_record.sequence();
    }
    assert_eq!(first.dropped(), 0);
    assert_eq!(second.dropped(), 0);
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
