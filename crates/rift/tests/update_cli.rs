//! Proves `rift update` fails typed on an unreachable network, never
//! panicking on the async runtime and never drawing progress off a terminal.

use std::process::Command;

/// A proxy address nothing listens on: port 9 (discard) on the loopback.
///
/// Routing the updater's HTTP transport through it makes the release
/// lookup fail fast and deterministically, with no live network touched.
const DEAD_PROXY: &str = "http://127.0.0.1:9";

#[test]
fn update_reports_a_typed_failure_off_the_runtime() {
    let output = Command::new(env!("CARGO_BIN_EXE_rift"))
        .arg("update")
        .env("HTTP_PROXY", DEAD_PROXY)
        .env("HTTPS_PROXY", DEAD_PROXY)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .expect("the update invocation must run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "an unreachable release lookup must fail typed, stderr: {stderr}"
    );
    assert!(
        stderr.contains("rift: error["),
        "the failure must carry a registry code, stderr: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "the updater must never panic on the runtime, stderr: {stderr}"
    );
    assert!(
        !stderr.contains("Checking the latest rift release"),
        "stage lines are drawn on a terminal only, stderr: {stderr}"
    );
}
