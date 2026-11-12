//! CLI-created project profiles through the binary MCP proxy.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::workspace_client::query;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::query::{QueryAnswer, QueryLanguage, QueryParams};
use rift_protocol::read::SymbolId;
use rift_protocol::symbol_read::CapturedView;
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
    let lib_uri = format!("file://{}/lib.rs", root.display());
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
            r#"    *'"method":"textDocument/references"'*)"#,
            r#"    *'"method":"textDocument/references"'*)
      case "$uri:$body" in
        "$CALLEE_URI:"*'"position":{"line":0,"character":7}'*) ;;
        "$CALLEE_URI:"*'"position":{"line":1,"character":4}'*)
          frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":1,\"character\":4},\"end\":{\"line\":1,\"character\":10}}}]}"
          continue ;;
        "$CALLER_URI:"*'"position":{"line":1,"character":7}'*)
          frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":1,\"character\":7},\"end\":{\"line\":1,\"character\":13}}}]}"
          continue ;;
        *)
          frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
          continue ;;
      esac"#,
        )
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
        "pwd > '{}'\nCALLER_URI='{caller_uri}'\nCALLEE_URI='{lib_uri}'\n{script}",
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
    require_success(&output, "configure profile engine")?;
    let configuration = root.join("rift.toml");
    let original = fs::read_to_string(&configuration)?;
    let retry = if profile.is_some() {
        "profiles.docs.languages.rust.lsp.retry"
    } else {
        "languages.rust.lsp.retry"
    };
    let source = if profile.is_some() {
        "\n[profiles.docs.source]\ninclude = [\"Cargo.toml\", \"lib.rs\", \"caller.rs\"]\n[profiles.docs.languages.toml]\nenabled = false\n"
    } else {
        ""
    };
    fs::write(
        configuration,
        format!(
            "{original}\n[{retry}]\nattempts = 3\ndelay = \"250ms\"\ndelay_limit = \"2s\"\n{source}"
        ),
    )?;
    Ok(())
}

fn search() -> TestResult<Value> {
    let selected = |scope: &str| {
        format!(
            r#"{{ SERVICE <{}> {{ ?hit :query "beacon"; :target "symbol"; :scope "{scope}"; :paths '{{"include":["lib.rs"]}}'^^rdf:JSON; :order "path"; :include '["source"]'^^rdf:JSON; :symbol ?symbol; :source ?source; :path ?path . ?symbol :name ?name . }} }}"#,
            "urn:rift:vocabulary:search",
        )
    };
    Ok(serde_json::to_value(QueryParams {
        query: format!(
            "PREFIX : <{}> PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> SELECT ?symbol ?source ?path ?name WHERE {{ {} UNION {} }} ORDER BY ?path ?symbol",
            rift_search::sparql::VOCABULARY_NAMESPACE,
            selected("local"),
            selected("local:docs"),
        ),
        language: QueryLanguage::Sparql,
    })?)
}

#[derive(Debug)]
struct ProfileSymbol {
    id: SymbolId,
    source: String,
}

fn retained_page(answer: &Value, page: u64) -> TestResult<Value> {
    let retained: QueryAnswer = serde_json::from_value(answer["answer"].clone())?;
    let view: CapturedView = serde_json::from_value(answer["view"].clone())?;
    let query = rift_search::sparql::continuation_query(&retained, &view.id, page)?;
    Ok(serde_json::to_value(QueryParams {
        query,
        language: QueryLanguage::Sparql,
    })?)
}

fn hit(answer: &Value) -> TestResult<ProfileSymbol> {
    let rows = query::rows(answer)?;
    assert_eq!(rows.len(), 1, "one projected symbol: {answer}");
    Ok(ProfileSymbol {
        id: serde_json::from_value(json!(query::iri(&rows[0], "symbol")?))?,
        source: query::literal(&rows[0], "source")?.to_owned(),
    })
}

async fn symbol_read(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    hit: &ProfileSymbol,
) -> TestResult<Value> {
    let id = &hit.id;
    let arguments = json!({
        "id":id,
    });
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
    let nodes = proxied_call(
        client,
        "nodes",
        &json!({"symbol":id, "revision":answer["revision"]}),
    )
    .await?;
    assert_eq!(
        nodes["nodes"][0]["source"].as_str(),
        Some(hit.source.as_str()),
        "{nodes}"
    );
    assert_eq!(
        declarations[0].path.as_ref().map(|path| path.0.as_str()),
        Some("lib.rs"),
        "{answer}"
    );
    Ok(answer)
}

async fn named_missing_read(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    hit: &ProfileSymbol,
) -> TestResult {
    let found = &hit.id;
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
    hit: &ProfileSymbol,
    logs: &Path,
) -> TestResult {
    let seed = oxrdf::NamedNodeRef::new(hit.id.as_str())?;
    let arguments = json!({"query":format!(
        "SELECT DISTINCT ?symbol {{ ?symbol :references {seed} }}"
    )});
    let answer = match proxied_call(client, "search", &arguments).await {
        Ok(answer) => answer,
        Err(error) => {
            let main_requests = fs::read_to_string(logs.join("main.requests"));
            let docs_requests = fs::read_to_string(logs.join("docs.requests"));
            return Err(format!("profile query failed: {error}; main requests: {main_requests:?}; docs requests: {docs_requests:?}").into());
        }
    };
    let rows = query::rows(&answer)?;
    assert_eq!(rows.len(), 1, "{answer}");
    let caller = query::iri(&rows[0], "symbol")?;
    let nodes = proxied_call(client, "nodes", &json!({"symbol":caller})).await?;
    assert_eq!(
        nodes["nodes"][0]["source"], "pub fn caller() -> u32 {\n    beacon()\n}",
        "{nodes}"
    );
    assert_eq!(
        SymbolIdentity::parse(query::iri(&rows[0], "symbol")?)?.owner(),
        SymbolIdentity::parse(hit.id.as_str())?.owner(),
        "{answer}"
    );
    Ok(())
}

fn engine_requests(logs: &Path) -> TestResult<Vec<Value>> {
    let mut requests = Vec::new();
    for name in ["main", "docs"] {
        for line in fs::read_to_string(logs.join(format!("{name}.requests")))?.lines() {
            let message: Value = serde_json::from_str(line)?;
            if message.get("id").is_some() {
                requests.push(json!({"engine": name, "request": message}));
            }
        }
    }
    Ok(requests)
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

async fn require_profile_source_scope(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult {
    let arguments = serde_json::to_value(QueryParams {
        query:
            "PREFIX : <urn:rift:vocabulary:> SELECT ?symbol { ?symbol a :Symbol; :language 'toml' }"
                .to_owned(),
        language: QueryLanguage::Sparql,
    })?;
    let answer = proxied_call(client, "search", &arguments).await?;
    assert!(
        query::rows(&answer)?.is_empty(),
        "profile source includes only the configured Rust files: {answer}"
    );
    Ok(())
}

async fn require_profile_scope(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult {
    let arguments = query::retrieval(&serde_json::from_value(json!({
        "query":"beacon", "target":"symbol", "scope":"local:docs",
        "paths":{"include":["lib.rs"]}, "include":["source"],
    }))?)?;
    let scoped = proxied_call(client, "search", &serde_json::to_value(arguments)?).await?;
    assert!(
        hit(&scoped)?
            .id
            .as_str()
            .starts_with("rift://symbol/local:docs/"),
        "profile identities retain their owner: {scoped}"
    );
    require_profile_pattern(client).await?;
    let extra_arguments = query::retrieval(&serde_json::from_value(json!({
        "query":"beacon", "target":"symbol", "scope":"local@docs"
    }))?)?;
    let extra = within(
        "unregistered extra scope",
        client.call_tool(
            rmcp::model::CallToolRequestParams::new("search").with_arguments(
                serde_json::from_value(serde_json::to_value(extra_arguments)?)?,
            ),
        ),
    )
    .await?;
    assert!(
        !extra
            .as_ref()
            .is_ok_and(|answer| answer.is_error != Some(true)),
        "a profile does not register an extra workspace: {extra:?}"
    );
    Ok(())
}

async fn require_profile_pattern(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult {
    let arguments = query::retrieval(&serde_json::from_value(json!({
        "pattern":"pub fn beacon", "target":"symbol", "scope":"local:docs",
        "paths":{"include":["lib.rs"]}, "include":["source"],
    }))?)?;
    let patterned = proxied_call(client, "search", &serde_json::to_value(arguments)?).await?;
    let symbol = hit(&patterned)?;
    assert_eq!(
        SymbolIdentity::parse(symbol.id.as_str())?.owner(),
        &SymbolOwner::ProfileLocal {
            name: "docs".to_owned()
        }
    );
    symbol_read(client, &symbol).await?;
    Ok(())
}

/// A root-defined profile keeps one owner and one configured engine.
async fn require_original_profile_hit(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    hit: &ProfileSymbol,
    owners: &mut BTreeSet<&'static str>,
) -> TestResult {
    let id = &hit.id;
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
    assert_eq!(hit.source.as_str(), expected, "{hit:?}");
    symbol_read(client, hit).await?;
    if matches!(identity.owner(), SymbolOwner::ProfileLocal { .. }) {
        named_missing_read(client, hit).await?;
    }
    Ok(())
}

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
        "[source]\ninclude = [\"Cargo.toml\", \"lib.rs\", \"caller.rs\", \"docs/lib.rs\", \"docs/caller.rs\"]\n[search.text]\ninclude = [\"Cargo.toml\"]\n[languages.toml]\nenabled = false\n[search.query]\npage_size = 1\n[global]\nenabled = false\n",
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
    require_profile_source_scope(&docs_client).await?;
    docs_client.cancel().await?;
    require_profile_scope(&client).await?;
    let first = proxied_call(&client, "search", &search()?).await?;
    assert_eq!(first["pagination"]["total_pages"], 2, "{first}");
    let view = first["view"].clone();
    let _: rift_protocol::symbol_read::CapturedView = serde_json::from_value(view.clone())
        .map_err(|error| format!("merged search view absent or invalid: {error}; {first}"))?;
    let second_arguments = serde_json::to_value(QueryParams {
        query: first["pagination"]["next_query"]
            .as_str()
            .ok_or("second query absent")?
            .to_owned(),
        language: QueryLanguage::Sparql,
    })?;
    let second = proxied_call(&client, "search", &second_arguments).await?;
    assert_eq!(second["view"], view, "pagination keeps original expiry");
    let hits = [hit(&first)?, hit(&second)?];
    let mut owners = BTreeSet::new();
    for hit in &hits {
        require_original_profile_hit(&client, hit, &mut owners).await?;
        caller_read(&client, hit, logs.path()).await?;
    }
    assert_eq!(owners, BTreeSet::from(["main", "docs"]));
    let requests = engine_requests(logs.path())?;
    fs::write(root.join("lib.rs"), main_source.replace("11", "33"))?;
    fs::write(docs.join("lib.rs"), docs_source.replace("22", "44"))?;
    let held_first = proxied_call(&client, "search", &retained_page(&first, 0)?).await?;
    let held_second = proxied_call(&client, "search", &second_arguments).await?;
    assert_eq!(held_first, first, "captured source and expiry stay fixed");
    assert_eq!(held_second, second, "both backend views stay fixed");
    assert_eq!(
        engine_requests(logs.path())?,
        requests,
        "held reads must not issue engine requests"
    );
    require_engine_root(logs.path(), "main", root).await?;
    require_engine_root(logs.path(), "docs", &docs).await?;
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn profile_engine_references_keep_original_declarations_and_callers() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let logs = tempfile::tempdir()?;
    let script = engine_script(&root, logs.path(), "references")?;
    let configuration = serde_json::from_value(json!({ "command": ["sh", "-c", script] }))?;
    let key = rift_server::LspProcessKey::named("rust");
    let pool = rift_server::EnginePool::new(
        &root,
        std::collections::BTreeMap::from([(key.clone(), configuration)]),
        std::collections::BTreeMap::from([("rust".to_owned(), key.clone())]),
    );
    let lib = format!("file://{}/lib.rs", root.display());
    let caller = format!("file://{}/caller.rs", root.display());
    let inputs = [
        (format!("file://{}/unknown.rs", root.display()), 0, 7),
        (lib.clone(), 1, 4),
        (caller.clone(), 1, 7),
        (lib.clone(), 0, 7),
    ];
    let answers = pool
        .engine_by_key(&key)
        .ok_or("configured Rust engine missing")?
        .request(|session| {
            let inputs = inputs.clone();
            Box::pin(async move {
                let mut answers = Vec::new();
                for (uri, line, character) in inputs {
                    let path = rift_core::ProjectPath::new(
                        uri.rsplit('/').next().expect("controlled file URI"),
                    )?;
                    let position = serde_json::from_value(json!({
                        "line": line, "character": character,
                    }))
                    .expect("controlled reference position");
                    answers.push(session.references(&path, position).await?);
                }
                Ok(answers)
            })
        })
        .await;
    pool.shutdown().await;
    let answers = serde_json::to_value(answers?)?;
    let location = |uri: &str, line: u32, start: u32, end: u32| {
        json!({
            "uri": uri,
            "range": {
                "start": { "line": line, "character": start },
                "end": { "line": line, "character": end },
            },
        })
    };
    assert_eq!(
        answers,
        json!([
            [],
            [location(&lib, 1, 4, 10)],
            [location(&caller, 1, 7, 13)],
            [location(&lib, 0, 7, 13), location(&caller, 2, 4, 10)],
        ]),
    );
    Ok(())
}
