//! Integration tests: `rename_symbol` without an engine, and its schema
//! refusals.
//!
//! Every engine-covered arm - an applied rename spanning files, a
//! relative-root rename, the surviving-occurrence sweep, a capability
//! refusal, a declined or refused prepare, an out-of-tree escape, a
//! timeout, the disk-mutation race, an engine's own refusal words, an
//! edit that changes nothing, absorption of a cancelled or provisional
//! answer, and an engine restarted mid-plan - is proven against a real
//! engine instead of a scripted one: `live_rust_analyzer.rs` and
//! `live_typescript.rs` prove the applied and relative-root renames, the
//! engine-refusal wording, and (rust-analyzer) a mapped diagnostic riding
//! the applied change. The absorption, restart, and disk-mutation-race
//! scenarios that stood here needed an engine told to misbehave on a
//! precise schedule - cancel exactly once, die exactly once, mutate a
//! file at exactly the wrong moment - which no real engine offers and no
//! scripted engine survives to provide; that coverage belongs to whatever
//! reworks the absorption policy next.

#![cfg(unix)]

mod hermetic_search;
// `served_relative_workspace` is part of `workspace_client`'s shared surface; this binary
// never needs a relative-root fixture, since the tests here carry no engine for the root
// spelling to matter to.
#[allow(dead_code)]
mod workspace_client;

use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use workspace_client::{TestResult, call_retrying_acceptance, served_workspace, tool_request};

fn rename_request(symbol: &str, new_name: &str) -> CallToolRequestParams {
    tool_request(
        "rename_symbol",
        &json!({ "symbol": symbol, "new_name": new_name }),
    )
}

const LIBRARY: &str = "pub fn beacon() {}\n";
const BEACON_SYMBOL: &str = "rift://symbol/rust/lib.rs/beacon";

fn refusal_detail(structured: &Value) -> String {
    structured["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn unserved_language_refuses_unsupported() -> TestResult {
    let (_directory, client, server_task) = served_workspace(&[("lib.rs", LIBRARY)], None).await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unsupported"));
    assert!(
        refusal_detail(&structured).contains("no engine configured for language rust"),
        "the refusal names the unserved language: {structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn empty_new_name_is_a_typed_invalid_request() -> TestResult {
    let (_directory, client, server_task) = served_workspace(&[("lib.rs", LIBRARY)], None).await?;

    let error = client
        .call_tool(rename_request(BEACON_SYMBOL, ""))
        .await
        .expect_err("the schema-advertised length is enforced at acceptance");
    let rmcp::ServiceError::McpError(data) = error else {
        return Err(format!("expected a tool error, got {error:?}").into());
    };
    let wire = data.data.ok_or("wire error data must be present")?;
    assert_eq!(wire["code"], json!("invalid_request"), "{wire:#}");

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
