//! Runtime shutdown sends stop records through the log provider.

use rift_tracing::TracingRuntime;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn shutdown_sends_records_before_closing_export() -> TestResult {
    let (runtime, drain) = TracingRuntime::builder().capture("trace").install()?;
    rift_tracing::info!(target: "rift", "recorded before runtime shutdown");
    let shutdown = runtime.shutdown().await;
    let mut drain = drain.ok_or("a capture filter returns a drain")?;
    let records = std::iter::from_fn(|| drain.try_recv_record().ok()).collect::<Vec<_>>();
    // XFAIL: https://github.com/volarized/rift/issues/585
    // Keep the recorded platform, error, and stop-stage fields together.
    if cfg!(all(windows, target_arch = "aarch64"))
        && matches!(shutdown, Err(rift_tracing::ExportShutdownError::TimedOut))
        && records.iter().any(|record| {
            record.message() == "stop stage ended"
                && record.fields().contains("\"stage\":\"otlp export\"")
                && record.fields().contains("\"outcome\":\"timeout\"")
                && record.fields().contains("\"remaining\":\"0ns\"")
        })
    {
        eprintln!("XFAIL https://github.com/volarized/rift/issues/585: {shutdown:?}");
    } else {
        shutdown?;
    }
    assert!(
        records
            .iter()
            .any(|record| record.message() == "stop stage ended"),
        "{records:?}"
    );
    Ok(())
}
