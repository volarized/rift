//! Profiles an extra project defines in its own `rift.toml`, through the binary MCP proxy.

use std::collections::BTreeMap;

use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::search::SearchHitTarget;
use rift_protocol::search_read::SearchReadResult;
use rift_protocol::symbol_read::GetSymbolResult;
use serde_json::{Value, json};

use crate::harness::{
    StopOnDrop, TestResult, await_workspace_ready, laid_out_workspace, proxied_call, proxy_client,
    within,
};

const PRIMARY_SOURCE: &str = "pub fn beacon() -> u32 { 11 }";
const EXTRA_SOURCE: &str = "pub fn beacon() -> u32 { 22 }";
const PROFILE_SOURCE: &str = "pub fn beacon() -> u32 { 33 }";
const MANIFEST: &str =
    "[package]\nname=\"profile_fixture\"\nversion=\"1.0.0\"\n[lib]\npath=\"lib.rs\"\n";

/// Each `beacon` hit a search answers, keyed by its owner's local scope.
async fn beacons(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    scope: &str,
) -> TestResult<BTreeMap<String, (String, Option<String>)>> {
    let arguments = json!({
        "query":"beacon", "target":"symbol", "scope":scope,
        "paths":{"include":["lib.rs"]}, "include":["source"], "limit":10,
    });
    let answer = proxied_call(client, "search", &arguments).await?;
    let result: SearchReadResult = serde_json::from_value(answer.clone())
        .map_err(|error| format!("invalid search result: {error}; {answer}"))?;
    let SearchReadResult::Results(result) = result else {
        return Err(format!("search unavailable for {scope}: {answer}").into());
    };
    let mut hits = BTreeMap::new();
    for hit in result.results {
        let SearchHitTarget::Symbol { symbol } = &hit.hit else {
            return Err(format!("expected symbol hit: {answer}").into());
        };
        let id = symbol
            .id
            .as_ref()
            .ok_or_else(|| format!("canonical symbol absent for {scope}: {answer}"))?;
        let owner = SymbolIdentity::parse(id.as_str())?.owner().local_scope()?;
        let previous = hits.insert(owner, (id.as_str().to_owned(), hit.source));
        assert!(previous.is_none(), "each project answers once: {answer}");
    }
    Ok(hits)
}

/// One extra project registration serves its primary tree and each of its own profiles.
#[tokio::test]
async fn extra_project_profiles_serve_distinct_scopes_through_one_connection() -> TestResult {
    let extra = laid_out_workspace(
        &[
            ("Cargo.toml", MANIFEST),
            ("lib.rs", &format!("{EXTRA_SOURCE}\n")),
            ("docs/Cargo.toml", MANIFEST),
            ("docs/lib.rs", &format!("{PROFILE_SOURCE}\n")),
        ],
        "[profiles.docs]\nroot = 'docs'\n",
    )?;
    let extra_root = extra.path().canonicalize()?;
    let docs = extra_root.join("docs");
    let registration = format!("[mcp.extra]\nother = '{}'\n", extra_root.to_string_lossy());
    let primary = laid_out_workspace(
        &[
            ("Cargo.toml", MANIFEST),
            ("lib.rs", &format!("{PRIMARY_SOURCE}\n")),
        ],
        &registration,
    )?;
    let root = primary.path();
    let _primary_stop = StopOnDrop::new(root);
    let _extra_stop = StopOnDrop::new(&extra_root);
    let _docs_stop = StopOnDrop::new(&docs);
    for project in [&extra_root, &docs] {
        let warm = proxy_client(project).await?;
        await_workspace_ready(&warm).await?;
        warm.cancel().await?;
    }
    let client = proxy_client(root).await?;
    await_workspace_ready(&client).await?;

    let extra_hits = beacons(&client, "local@other").await?;
    assert_eq!(
        extra_hits.keys().collect::<Vec<_>>(),
        ["local@other"],
        "the extra project leaves its profile folder to the profile"
    );
    assert_eq!(extra_hits["local@other"].1.as_deref(), Some(EXTRA_SOURCE));
    let profile_hits = beacons(&client, "local@other:docs").await?;
    let (profile_id, profile_source) = &profile_hits["local@other:docs"];
    assert_eq!(profile_hits.len(), 1);
    assert_eq!(profile_source.as_deref(), Some(PROFILE_SOURCE));
    assert!(
        profile_id.starts_with("rift://symbol/local@other:docs/"),
        "{profile_id}"
    );
    let all = beacons(&client, "all").await?;
    assert_eq!(
        all.keys().collect::<Vec<_>>(),
        ["local", "local@other", "local@other:docs"]
    );
    assert_eq!(all["local"].1.as_deref(), Some(PRIMARY_SOURCE));

    let found = proxied_call(
        &client,
        "get_symbol",
        &json!({"id":profile_id, "include":["source"]}),
    )
    .await?;
    let found: GetSymbolResult = serde_json::from_value(found)?;
    let GetSymbolResult::Found {
        symbol,
        declarations,
        ..
    } = found
    else {
        return Err("extra profile symbol unavailable".into());
    };
    assert_eq!(
        symbol
            .id
            .as_ref()
            .map(rift_protocol::read::SymbolId::as_str),
        Some(profile_id.as_str())
    );
    assert_eq!(
        SymbolIdentity::parse(profile_id)?.owner(),
        &SymbolOwner::NamedProfileLocal {
            project: "other".to_owned(),
            name: "docs".to_owned(),
        }
    );
    assert_eq!(declarations[0].source.as_deref(), Some(PROFILE_SOURCE));

    for scope in ["local@other:api", "local:docs"] {
        let refused = within(
            "unregistered profile scope",
            client.call_tool(
                rmcp::model::CallToolRequestParams::new("search").with_arguments(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "query":"beacon", "target":"symbol", "scope":scope,
                    }))?,
                ),
            ),
        )
        .await?;
        assert!(refused.is_err(), "{scope} names no registered profile");
    }
    client.cancel().await?;
    Ok(())
}
