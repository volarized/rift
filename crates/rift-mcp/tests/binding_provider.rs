//! Proves the binding provider reaches the caller-visible read surface:
//! `get_symbol` lists a `binding` contribution beside `syntax` for a
//! declaration another unit references.

mod hermetic_search;
// `served_relative_workspace` is part of `workspace_client`'s shared surface; this
// binary drives no engine, so the root spelling never matters here.
#[allow(dead_code)]
mod workspace_client;

use serde_json::{Value, json};
use workspace_client::{TestResult, call_retrying_acceptance, served_workspace, tool_request};

/// The providers named by one hit's `contributions` list.
fn contribution_providers(hit: &Value) -> Vec<&str> {
    hit["symbol"]["contributions"]
        .as_array()
        .expect("hit carries a contributions list")
        .iter()
        .map(|contribution| {
            contribution["provider"]
                .as_str()
                .expect("contribution names its provider")
        })
        .collect()
}

#[tokio::test]
async fn get_symbol_lists_the_binding_provider_beside_syntax() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(
        &[
            (
                "src/lib.rs",
                "mod run;\nuse run::helper;\npub fn beacon() {\n    helper();\n}\n",
            ),
            ("src/run.rs", "pub fn helper() {}\n"),
        ],
        None,
    )
    .await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("get_symbol", &json!({"name": "helper"})),
    )
    .await?;
    let hits = structured["hits"]
        .as_array()
        .ok_or("get_symbol must return hits")?;
    let hit = hits
        .iter()
        .find(|hit| hit["symbol"]["name"] == json!("helper"))
        .ok_or("the helper declaration must be a hit")?;
    let providers = contribution_providers(hit);
    assert!(
        providers.contains(&"syntax"),
        "the declaration keeps its syntax contribution: {providers:?}"
    );
    assert!(
        providers.contains(&"binding"),
        "the binding provider's definition joins the same record: {providers:?}"
    );

    client.cancel().await?;
    Ok(())
}
