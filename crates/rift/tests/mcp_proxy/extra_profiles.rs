//! Profiles an extra project defines in its own `rift.toml`, through the binary MCP proxy.

use std::collections::BTreeMap;

use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::symbol_read::GetSymbolResult;
use serde_json::json;

use crate::workspace_client::query;

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
    let parameters = serde_json::from_value(json!({
        "query":"beacon", "target":"symbol", "scope":scope,
        "paths":{"include":["lib.rs"]}, "include":["source"], "limit":10,
    }))?;
    let arguments = query::retrieval(&parameters)?;
    let answer = crate::workspace_client::query_after_population(client, &arguments).await?;
    let mut hits = BTreeMap::new();
    for hit in query::rows(&answer)? {
        let id = query::iri(hit, "symbol")?;
        let owner = SymbolIdentity::parse(id)?.owner().local_scope()?;
        let source = query::literal(hit, "source").ok().map(str::to_owned);
        let previous = hits.insert(owner, (id.to_owned(), source));
        assert!(previous.is_none(), "each project answers once: {answer}");
    }
    Ok(hits)
}

/// Asserts that `scope` is refused with the accepted scopes of the connection.
async fn require_unregistered_scope(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    scope: &str,
) -> TestResult {
    let scope = oxrdf::Literal::from(scope);
    let arguments = json!({"query": format!(
        "SELECT ?hit {{ SERVICE :search {{ ?hit :query 'beacon'; :target 'symbol'; :scope {scope}; :index ?index }} }}"
    )});
    let response = within(
        "unregistered profile scope",
        client.call_tool(crate::workspace_client::tool_request("search", &arguments)),
    )
    .await?;
    let Err(rmcp::ServiceError::McpError(failure)) = response else {
        return Err(format!("expected unregistered scope refusal: {response:?}").into());
    };
    assert!(
        failure.message.contains("local@other"),
        "{scope}: {failure:?}"
    );
    assert!(
        failure.message.contains("local@other:docs"),
        "{scope}: {failure:?}"
    );
    Ok(())
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
    let primary_hits = beacons(&client, "local").await?;
    assert_eq!(primary_hits["local"].1.as_deref(), Some(PRIMARY_SOURCE));
    let all = crate::workspace_client::query_after_population(&client, &rift_protocol::query::QueryParams {
        query: "SELECT (COUNT(?hit) AS ?count) { SERVICE :search { ?hit :query 'beacon'; :target 'file'; :scope 'all' } }".to_owned(),
        language: rift_protocol::query::QueryLanguage::Sparql,
    }).await?;
    let [count] = query::rows(&all)? else {
        return Err(format!("one all-scope count: {all}").into());
    };
    assert_eq!(query::literal(count, "count")?, "3");

    let found = proxied_call(&client, "get_symbol", &json!({"id":profile_id})).await?;
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
    assert_eq!(declarations.len(), 1);

    let nodes = proxied_call(&client, "nodes", &json!({"symbol":profile_id})).await?;
    let item = &nodes["nodes"][0];
    assert_eq!(
        nodes["symbol"].as_str(),
        Some(profile_id.as_str()),
        "{nodes}"
    );
    assert_eq!(item["index"].as_u64(), Some(0), "{nodes}");
    assert_eq!(item["source"].as_str(), Some(PROFILE_SOURCE), "{nodes}");

    for scope in ["local@other:api", "local:docs"] {
        require_unregistered_scope(&client, scope).await?;
    }
    client.cancel().await?;
    Ok(())
}
