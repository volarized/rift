//! Proves the binding provider reaches the caller-visible read surface: `get_symbol`'s
//! `helper` hit carries the binding provider's namespaced extension, published beside
//! syntax's own facts for a declaration another unit references.

mod hermetic_search;
// `served_relative_workspace` is part of `workspace_client`'s shared surface; this
// binary drives no engine, so the root spelling never matters here.
#[allow(dead_code)]
mod workspace_client;

use serde_json::{Value, json};
use workspace_client::{TestResult, call_retrying_acceptance, served_workspace, tool_request};

/// The reverse-domain key `crates/rift-binding/src/publish.rs`'s `BindingPublisher`
/// carries its facts under (`BINDING_EXTENSION_KEY`). The binding provider is the only
/// Contribution that publishes it, so its presence in `extensions` is what the wire
/// still shows now that `contributions` left the wire: `Symbol.extensions` merges every
/// selected Contribution's namespaced facts regardless of which fields it carries.
const BINDING_EXTENSION_KEY: &str = "org.rift.binding";

#[tokio::test]
async fn get_symbol_shows_the_binding_provider_through_extensions() -> TestResult {
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
    assert!(
        has_binding_extension(hit),
        "the binding provider's definition Contribution carries {BINDING_EXTENSION_KEY} \
         in the merged extensions: {hit}"
    );

    client.cancel().await?;
    Ok(())
}

/// Whether one hit's symbol carries the binding provider's namespaced extension key.
fn has_binding_extension(hit: &Value) -> bool {
    hit["symbol"]["extensions"]
        .get(BINDING_EXTENSION_KEY)
        .is_some()
}
