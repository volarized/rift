//! Integration tests: `remove_symbol` without an engine configured.
//!
//! The engine-covered arms - a standing reference refusing the removal, an
//! absent references capability, an engine that refuses the check itself,
//! and the disk-mutation race between the plan and the apply - are proven
//! against a real engine instead of a scripted one:
//! `live_rust_analyzer.rs` proves the standing-reference refusal end to
//! end. The disk-mutation race specifically needed an engine that could be
//! told to mutate a file at the exact moment it answers a reference check;
//! no real engine offers that control point, and no scripted engine
//! survives to provide it, so that scenario is dropped with this reason.

#![cfg(unix)]

mod hermetic_search;
// `served_workspace` is part of `workspace_client`'s shared surface; this binary's one test
// needs only the relative-root fixture.
#[allow(dead_code)]
mod workspace_client;

use std::fs;

use serde_json::json;
use workspace_client::{
    TestResult, call_retrying_acceptance, served_relative_workspace, tool_request,
};

/// The library and the file referencing it, both served by the engine.
const LIBRARY: &str = "pub fn beacon() {}\n";
const BEACON_SYMBOL: &str = "rift://symbol/rust/lib.rs/beacon";

fn coded_findings<'summary>(
    structured: &'summary serde_json::Value,
    code: &str,
) -> Vec<&'summary serde_json::Value> {
    structured["summary"]["diagnostics"]
        .as_array()
        .map(|findings| {
            findings
                .iter()
                .filter(|finding| finding["code"] == json!(code))
                .collect()
        })
        .unwrap_or_default()
}

/// A workspace served under the CLI's own root spelling removes the same way one served
/// under an absolute path does: reads and writes below the root resolve against the process
/// working directory either way, so only the engine tier - addressed in `file://` URIs -
/// could tell the two spellings apart, and no engine is configured here.
#[tokio::test]
async fn remove_symbol_under_a_relative_root_applies_unchecked() -> TestResult {
    let (directory, client, server_task) =
        served_relative_workspace(&[("lib.rs", LIBRARY)], None).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "remove_symbol",
            &json!({ "symbol": BEACON_SYMBOL, "force": false }),
        ),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = coded_findings(&structured, "rift.remove.unchecked");
    assert_eq!(findings.len(), 1, "{structured:#}");
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no engine is configured for language rust"),
        "the warning must name the absent engine: {findings:#?}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "",
        "the sole declaration is removed"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
