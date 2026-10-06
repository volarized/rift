//! Runtime shutdown sends stop records through the log provider.

use rift_tracing::TracingRuntime;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn shutdown_sends_records_before_closing_export() -> TestResult {
    let (runtime, drain) = TracingRuntime::builder().capture("trace").install()?;
    rift_tracing::info!(target: "rift", "recorded before runtime shutdown");
    runtime.shutdown().await?;
    let mut drain = drain.ok_or("a capture filter returns a drain")?;
    let records = std::iter::from_fn(|| drain.try_recv_record().ok()).collect::<Vec<_>>();
    assert!(
        records
            .iter()
            .any(|record| record.message() == "stop stage ended"),
        "{records:?}"
    );
    Ok(())
}
