//! CLI-created project profiles through the binary MCP proxy.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::search::{SearchHit, SearchHitTarget};
use rift_protocol::search_read::SearchReadResult;
use rift_protocol::symbol_read::GetSymbolResult;
use serde_json::{Value, json};

use crate::harness::{
    StopOnDrop, TestResult, await_workspace_ready, laid_out_workspace, proxied_call, proxy_client,
    require_success, run_rift, within,
};

#[path = "../../../rift-mcp/tests/fake_engine/script.rs"]
mod scripted_engine;

const MAIN_SOURCE: &str = "pub fn beacon() -> u32 { 11 }\n";
const DOCS_SOURCE: &str = "pub fn beacon() -> u32 { 22 }\n";
const CALLER_SOURCE: &str = "use super::beacon;\npub fn caller() -> u32 {\n    beacon()\n}\n";
const MANIFEST: &str =
    "[package]\nname=\"profile_fixture\"\nversion=\"1.0.0\"\n[lib]\npath=\"lib.rs\"\n";

fn engine_script(root: &Path, logs: &Path, name: &str) -> TestResult<String> {
    let root = root.canonicalize()?;
    let caller_uri = format!("file://{}/caller.rs", root.display());
    let cwd = logs.join(format!("{name}.cwd"));
    let requests = logs.join(format!("{name}.requests"));
    let progress = r#"frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}'
frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}'"#;
    // The existing responder owns framing and request dispatch. This fixture bounds
    // the bytes and frames it consumes, and records each selected project's requests.
    let script = scripted_engine::SCRIPTED_CALLS_ENGINE
        .replace("PROGRESS", progress)
        .replace("CALLEE_LINE", "0")
        .replace("CALLER_LINE", "2")
        .replace(
            "while IFS= read -r header; do",
            "frames=0; bytes=0\nwhile IFS= read -r header; do",
        )
        .replace(
            "  body=$(dd bs=1 count=\"$length\" 2>/dev/null)",
            &format!(
                "  frames=$((frames + 1)); bytes=$((bytes + length))\n  [ \"$frames\" -le 32 ] && [ \"$bytes\" -le 16384 ] || exit 91\n  body=$(dd bs=1 count=\"$length\" 2>/dev/null)\n  printf '%s\\n' \"$body\" >> '{}'",
                requests.display()
            ),
        );
    Ok(format!(
        "pwd > '{}'\nCALLER_URI='{caller_uri}'\n{script}",
        cwd.display()
    ))
}
async fn configure_engine(root: &Path, profile: Option<&str>, script: &str) -> TestResult {
    let script_argument = format!("--lsp-argument={script}");
    let mut arguments = vec![
        "config",
        "set",
        "--language=rust",
        "--lsp-program=sh",
        "--lsp-argument=-c",
        script_argument.as_str(),
    ];
    if profile.is_some() {
        arguments.extend(["--profile=docs", "--profile-root=docs"]);
    }
    let output = run_rift(root, &arguments).await?;
    require_success(&output, "configure profile engine")
}

fn search(page: u64, view: Option<&Value>) -> Value {
    let mut arguments = json!({
        "query":"beacon", "target":"symbol", "scope":"all",
        "paths":{"include":["lib.rs"]},
        "order":"path", "include":["source"], "limit":1, "page_index":page,
    });
    if let Some(view) = view {
        arguments["view"] = view.clone();
    }
    arguments
}

fn hit(answer: &Value) -> TestResult<SearchHit> {
    let result: SearchReadResult = serde_json::from_value(answer.clone())
        .map_err(|error| format!("invalid search result: {error}; {answer}"))?;
    let SearchReadResult::Results(result) = result else {
        return Err(format!("search unavailable: {answer}").into());
    };
    assert_eq!(
        result.results.len(),
        1,
        "one globally paginated result: {answer}"
    );
    assert_eq!(result.pagination.total_pages, 2, "{answer}");
    Ok(result
        .results
        .into_iter()
        .next()
        .expect("one search result"))
}

fn symbol_id(hit: &SearchHit) -> TestResult<&rift_protocol::read::SymbolId> {
    let SearchHitTarget::Symbol { symbol } = &hit.hit else {
        return Err(format!("expected symbol hit: {hit:?}").into());
    };
    symbol
        .id
        .as_ref()
        .ok_or_else(|| format!("canonical symbol absent: {hit:?}").into())
}

async fn symbol_read(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    hit: &SearchHit,
) -> TestResult<Value> {
    let id = symbol_id(hit)?;
    let arguments = json!({"id":id});
    let answer = proxied_call(client, "get_symbol", &arguments).await?;
    let result: GetSymbolResult = serde_json::from_value(answer.clone())
        .map_err(|error| format!("invalid symbol result: {error}; {answer}"))?;
    let GetSymbolResult::Found {
        symbol,
        declarations,
        ..
    } = result
    else {
        return Err(format!("symbol not found: request {arguments}; {answer}").into());
    };
    assert_eq!(symbol.id.as_ref(), Some(id), "{answer}");
    assert_eq!(declarations.len(), 1, "{answer}");
    assert_eq!(
        declarations[0].path.as_ref().map(|path| path.0.as_str()),
        Some("lib.rs"),
        "{answer}"
    );
    let nodes = proxied_call(client, "nodes", &json!({"symbol":id})).await?;
    let item = &nodes["nodes"][0];
    assert_eq!(nodes["symbol"].as_str(), Some(id.as_str()), "{nodes}");
    assert_eq!(item["index"].as_u64(), Some(0), "{nodes}");
    assert_eq!(item["source"].as_str(), hit.source.as_deref(), "{nodes}");
    Ok(answer)
}

async fn named_missing_read(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    hit: &SearchHit,
) -> TestResult {
    let found = symbol_id(hit)?;
    let identity = SymbolIdentity::parse(found.as_str())?;
    let mut path = identity.qualified_path().to_vec();
    "beacpn".clone_into(path.last_mut().ok_or("symbol path absent")?);
    let missing = rift_protocol::read::SymbolId::from_identity(&SymbolIdentity::new(
        identity.owner().clone(),
        identity.language().clone(),
        path,
    )?);
    let mut first = None;
    for _ in 0..2 {
        let arguments = json!({"id":missing});
        let response = within(
            "named missing symbol",
            client.call_tool(
                rmcp::model::CallToolRequestParams::new("get_symbol")
                    .with_arguments(serde_json::from_value(arguments)?),
            ),
        )
        .await??;
        assert_eq!(response.is_error, Some(true), "{response:?}");
        let value = response
            .structured_content
            .ok_or("typed missing answer absent")?;
        let result: GetSymbolResult = serde_json::from_value(value.clone())?;
        let GetSymbolResult::Missing {
            symbol_not_found, ..
        } = result
        else {
            return Err(format!("expected named symbol miss: {value}").into());
        };
        assert_eq!(symbol_not_found.id, missing, "{value}");
        assert!(
            symbol_not_found
                .alternatives
                .iter()
                .any(|alternative| &alternative.id == found),
            "{value}"
        );
        for alternative in symbol_not_found.alternatives {
            assert_eq!(
                SymbolIdentity::parse(alternative.id.as_str())?.owner(),
                identity.owner(),
                "{value}"
            );
        }
        if let Some(previous) = &first {
            assert_eq!(&value, previous, "a repeated miss answers the same");
        } else {
            first = Some(value);
        }
    }
    Ok(())
}

async fn caller_read(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    hit: &SearchHit,
    logs: &Path,
) -> TestResult {
    let arguments = json!({"traversal":{
        "seed":symbol_id(hit)?, "direction":"incoming", "depth":1,
    }, "target":"symbol", "include":["source"], "limit":5});
    let answer = match proxied_call(client, "search", &arguments).await {
        Ok(answer) => answer,
        Err(error) => {
            let main_requests = fs::read_to_string(logs.join("main.requests"));
            let docs_requests = fs::read_to_string(logs.join("docs.requests"));
            return Err(format!(
            "profile traversal failed: {error}; main requests: {main_requests:?}; docs requests: {docs_requests:?}"
        )
            .into());
        }
    };
    let result: SearchReadResult = serde_json::from_value(answer.clone())
        .map_err(|error| format!("invalid traversal result: {error}; {answer}"))?;
    let SearchReadResult::Results(result) = result else {
        return Err(format!("profile traversal unavailable: {answer}").into());
    };
    assert!(result.warnings.is_empty(), "{answer}");
    assert_eq!(result.results.len(), 1, "{answer}");
    let caller = &result.results[0];
    assert_eq!(
        caller.path.as_ref().map(|path| path.0.as_str()),
        Some("caller.rs"),
        "{answer}"
    );
    assert_eq!(
        caller.source.as_deref(),
        Some("pub fn caller() -> u32 {\n    beacon()\n}"),
        "{answer}"
    );
    assert_eq!(
        SymbolIdentity::parse(symbol_id(caller)?.as_str())?.owner(),
        SymbolIdentity::parse(symbol_id(hit)?.as_str())?.owner(),
        "{answer}"
    );
    Ok(())
}

async fn require_engine_root(logs: &Path, name: &str, root: &Path) -> TestResult {
    let requests = logs.join(format!("{name}.requests"));
    let expected_uri = format!("file://{}", root.canonicalize()?.display());
    let root_field = format!("\"rootUri\":\"{expected_uri}\"");
    let recorded = crate::wait_for(100, "profile engine initialization", || {
        fs::read_to_string(&requests)
            .ok()
            .filter(|text| text.contains(&root_field))
    })
    .await?;
    assert!(recorded.contains("workspaceFolders"), "{recorded}");
    let cwd = fs::read_to_string(logs.join(format!("{name}.cwd")))?;
    assert_eq!(Path::new(cwd.trim()).canonicalize()?, root.canonicalize()?);
    Ok(())
}

async fn require_profile_scope(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult {
    let scoped = proxied_call(
        client,
        "search",
        &json!({
            "query":"beacon", "target":"symbol", "scope":"local:docs",
            "paths":{"include":["lib.rs"]}, "include":["source"],
        }),
    )
    .await?;
    let scoped: SearchReadResult = serde_json::from_value(scoped)?;
    let SearchReadResult::Results(scoped) = scoped else {
        return Err("profile scope unavailable".into());
    };
    assert_eq!(scoped.results.len(), 1);
    assert!(
        symbol_id(&scoped.results[0])?
            .as_str()
            .starts_with("rift://symbol/local:docs/"),
        "profile identities carry the scope owner"
    );
    require_profile_pattern(client).await?;
    let extra = within(
        "unregistered extra scope",
        client.call_tool(
            rmcp::model::CallToolRequestParams::new("search").with_arguments(
                serde_json::from_value(json!({
                    "query":"beacon", "target":"symbol", "scope":"local@docs",
                }))?,
            ),
        ),
    )
    .await?;
    assert!(
        extra.is_err(),
        "a profile does not register an extra workspace"
    );
    Ok(())
}

async fn require_profile_pattern(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult {
    let patterned = proxied_call(
        client,
        "search",
        &json!({
            "pattern":"pub fn beacon", "target":"symbol", "scope":"local:docs",
            "paths":{"include":["lib.rs"]}, "include":["source"],
        }),
    )
    .await?;
    let patterned: SearchReadResult = serde_json::from_value(patterned)?;
    let SearchReadResult::Results(patterned) = patterned else {
        return Err("profile pattern unavailable".into());
    };
    assert_eq!(patterned.results.len(), 1);
    assert_eq!(
        SymbolIdentity::parse(symbol_id(&patterned.results[0])?.as_str())?.owner(),
        &SymbolOwner::ProfileLocal {
            name: "docs".to_owned()
        },
    );
    assert!(patterned.view.is_none());
    let id = symbol_id(&patterned.results[0])?;
    let found = proxied_call(client, "get_symbol", &json!({"id":id})).await?;
    let found: GetSymbolResult = serde_json::from_value(found)?;
    let GetSymbolResult::Found {
        symbol,
        declarations,
        ..
    } = found
    else {
        return Err("profile pattern symbol unavailable".into());
    };
    assert_eq!(symbol.id.as_ref(), Some(id));
    assert_eq!(declarations.len(), 1);
    let nodes = proxied_call(client, "nodes", &json!({"symbol":id})).await?;
    let item = &nodes["nodes"][0];
    assert_eq!(nodes["symbol"].as_str(), Some(id.as_str()), "{nodes}");
    assert_eq!(item["index"].as_u64(), Some(0), "{nodes}");
    assert_eq!(
        item["source"].as_str(),
        patterned.results[0].source.as_deref(),
        "{nodes}"
    );
    Ok(())
}

/// A root-defined profile keeps one owner and one configured engine.
#[tokio::test]
async fn cli_profiles_combine_search_preserve_views_and_select_each_project_engine() -> TestResult {
    let main_source = format!("{MAIN_SOURCE}mod caller;\n");
    let docs_source = format!("{DOCS_SOURCE}mod caller;\n");
    let directory = laid_out_workspace(
        &[
            ("Cargo.toml", MANIFEST),
            ("lib.rs", &main_source),
            ("caller.rs", CALLER_SOURCE),
            ("docs/Cargo.toml", MANIFEST),
            ("docs/lib.rs", &docs_source),
            ("docs/caller.rs", CALLER_SOURCE),
        ],
        "",
    )?;
    let root = directory.path();
    let docs = root.join("docs");
    let logs = tempfile::tempdir()?;
    let main_script = engine_script(root, logs.path(), "main")?;
    let docs_script = engine_script(&docs, logs.path(), "docs")?;
    configure_engine(root, None, &main_script).await?;
    configure_engine(root, Some("docs"), &docs_script).await?;
    assert!(
        !docs.join("rift.toml").exists(),
        "profiles use the root configuration"
    );
    let configuration = rift_server::load_configuration(root)?;
    assert!(configuration.mcp.extra.is_empty());
    assert_eq!(configuration.profiles["docs"].root, "docs");
    let _main_stop = StopOnDrop::new(root);
    let _docs_stop = StopOnDrop::new(&docs);
    let client = proxy_client(root).await?;
    await_workspace_ready(&client).await?;
    let docs_client = proxy_client(&docs).await?;
    await_workspace_ready(&docs_client).await?;
    docs_client.cancel().await?;
    require_profile_scope(&client).await?;
    let first = proxied_call(&client, "search", &search(0, None)).await?;
    let view = first["view"].clone();
    let _: rift_protocol::symbol_read::CapturedView = serde_json::from_value(view.clone())
        .map_err(|error| format!("merged search view absent or invalid: {error}; {first}"))?;
    let second = proxied_call(&client, "search", &search(1, Some(&view["id"]))).await?;
    assert_eq!(second["view"], view, "pagination keeps original expiry");
    let hits = [hit(&first)?, hit(&second)?];
    let mut owners = BTreeSet::new();
    for hit in &hits {
        let id = symbol_id(hit)?;
        let identity = SymbolIdentity::parse(id.as_str())?;
        let expected = match identity.owner() {
            SymbolOwner::Local => {
                owners.insert("main");
                MAIN_SOURCE.trim()
            }
            SymbolOwner::ProfileLocal { name } if name == "docs" => {
                owners.insert("docs");
                DOCS_SOURCE.trim()
            }
            owner => return Err(format!("unexpected profile owner: {owner:?}").into()),
        };
        assert_eq!(hit.source.as_deref(), Some(expected), "{hit:?}");
        symbol_read(&client, hit).await?;
        if matches!(identity.owner(), SymbolOwner::ProfileLocal { .. }) {
            named_missing_read(&client, hit).await?;
        }
    }
    assert_eq!(owners, BTreeSet::from(["main", "docs"]));
    fs::write(root.join("lib.rs"), main_source.replace("11", "33"))?;
    fs::write(docs.join("lib.rs"), docs_source.replace("22", "44"))?;
    let held_first = proxied_call(&client, "search", &search(0, Some(&view["id"]))).await?;
    let held_second = proxied_call(&client, "search", &search(1, Some(&view["id"]))).await?;
    assert_eq!(held_first, first, "captured source and expiry stay fixed");
    assert_eq!(held_second, second, "both backend views stay fixed");
    for hit in &hits {
        caller_read(&client, hit, logs.path()).await?;
    }
    require_engine_root(logs.path(), "main", root).await?;
    require_engine_root(logs.path(), "docs", &docs).await?;
    client.cancel().await?;
    Ok(())
}
