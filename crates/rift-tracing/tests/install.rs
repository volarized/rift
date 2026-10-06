//! Installing the runtime twice in one process.
//!
//! A global subscriber is process state, so this binary holds one test: nextest and
//! `cargo test` each run it in a process of its own.

use std::time::Duration;

use rift_tracing::TracingRuntime;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The second install returns [`rift_tracing::InstallError`] instead of panicking, and
/// the first installation keeps receiving every record.
#[test]
fn a_second_install_is_refused_and_the_first_keeps_its_records() -> TestResult {
    let (first, drain) = TracingRuntime::builder()
        .capture("trace")
        .stall_delay(Duration::from_secs(1))
        .install()?;
    let mut drain = drain.ok_or("a capture filter returns a drain")?;

    let Err(refused) = TracingRuntime::builder().capture("trace").install() else {
        return Err("a second install must be refused".into());
    };
    assert!(
        refused
            .to_string()
            .starts_with("tracing is already installed in this process: "),
        "{refused}"
    );
    assert!(std::error::Error::source(&refused).is_some());

    rift_tracing::info!(component = "test", "recorded after the refused install");
    let records = std::iter::from_fn(|| drain.try_recv_record().ok()).collect::<Vec<_>>();
    assert!(
        records
            .iter()
            .any(|record| record.message() == "recorded after the refused install"),
        "{records:?}"
    );
    assert!(
        records
            .iter()
            .any(|record| record.message() == "no Tokio runtime runs the stall report"),
        "{records:?}"
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(first.shutdown())?;
    Ok(())
}
