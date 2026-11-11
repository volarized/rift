//! Dependency call paths and captured package reads through the real stdio proxy.

use crate::workspace_client::query;
use serde_json::{Value, json};

use super::global_api::{FixtureOptions, GlobalFixture};
use super::harness::{
    FailureWindow, StopOnDrop, TestResult, await_workspace_ready, laid_out_workspace, proxied_call,
    proxy_client, require_success, run_rift,
};

const SOURCE: &str = "def greet(name: str) -> str:\n    return _helper(name)\n\ndef _helper(name: str) -> str:\n    return _inner(name)\n\ndef _inner(name: str) -> str:\n    return name\n";
const SOURCE_PATH: &str = ".venv/lib/python3.12/site-packages/greeting/core.py";
const RECORD_PATH: &str = ".venv/lib/python3.12/site-packages/greeting-1.0.0.dist-info/RECORD";
const FILES: &[(&str, &str)] = &[
    (
        "pyproject.toml",
        "[project]\nname='app'\nversion='0.1.0'\ndependencies=['greeting']\n[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['app']\n",
    ),
    (
        "uv.lock",
        "version = 1\n[[package]]\nname='app'\nversion='0.1.0'\nsource={editable='.'}\n[[package]]\nname='greeting'\nversion='1.0.0'\nsource={registry='https://pypi.org/simple'}\n",
    ),
    (".venv/pyvenv.cfg", "home=/usr/bin\nversion_info=3.12.4\n"),
    (
        ".venv/lib/python3.12/site-packages/greeting/__init__.py",
        "from greeting.core import greet\n",
    ),
    (
        RECORD_PATH,
        "greeting/__init__.py,,\ngreeting/core.py,,\ngreeting-1.0.0.dist-info/RECORD,,\n",
    ),
    (SOURCE_PATH, SOURCE),
    (
        "app.py",
        "from greeting.core import greet\n\ndef hello() -> str:\n    return greet('rift')\n",
    ),
];

fn row(answer: &Value) -> TestResult<&Value> {
    assert_eq!(answer["answer"]["result"], "bindings", "{answer}");
    let rows = answer["answer"]["bindings"]
        .as_array()
        .ok_or("SELECT bindings")?;
    assert_eq!(rows.len(), 1, "{answer}");
    assert_eq!(rows[0]["symbol"]["type"], "iri");
    Ok(&rows[0])
}

#[tokio::test]
async fn proxy_keeps_dependency_call_paths_and_original_source_bytes() -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        python_collection: true,
        withheld_features: &["declarations", "relationships"],
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[source]\nexclude=['.venv/**']\n[global]\nenabled=true\nendpoint='{}'\nattempts=1\nrequest_timeout='10s'\nconnect_timeout='5s'\n[languages.python.lsp]\nembedded='ty'\n",
        fixture.endpoint
    );
    let directory = laid_out_workspace(FILES, &configuration)?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let client = proxy_client(root).await?;
    await_workspace_ready(&client).await?;
    let declaration = proxied_call(
        &client,
        "search",
        &json!({"query":"SELECT ?symbol { ?symbol a :Symbol; :name 'hello' }"}),
    )
    .await?;
    let seed = row(&declaration)?["symbol"]["value"]
        .as_str()
        .ok_or("canonical local seed")?;
    let statement = format!(
        "SELECT ?symbol ?name ?unit ?start ?end ?source {{ \
          <{seed}> :calls/:calls/:calls ?symbol . ?symbol :name ?name . \
          ?node :of ?symbol; :index 0; :source ?source; :range [ :unit ?unit; :start ?start; :end ?end ] }}"
    );
    let answer = proxied_call(&client, "search", &json!({"query":statement})).await?;
    let projected = row(&answer)?;
    assert_eq!(projected["name"]["value"], "_inner");
    let identity = projected["symbol"]["value"]
        .as_str()
        .ok_or("canonical package declaration")?;
    let parsed = rift_protocol::identity::SymbolIdentity::parse(identity)?;
    assert!(
        matches!(parsed.owner(), rift_protocol::identity::SymbolOwner::Package { manager, registry, name } if manager == "pypi" && registry == "pypi.org" && name == "greeting")
    );
    let unit = query::iri(projected, "unit")?;
    let (owner, path) = rift_protocol::identity::parse_source_unit_identity(unit)?;
    assert_eq!(owner.as_ref(), Some(parsed.owner()));
    assert_eq!(path.as_str(), "greeting/core.py");
    let start = query::literal(projected, "start")?.parse::<usize>()?;
    let end = query::literal(projected, "end")?.parse::<usize>()?;
    let selected_bytes = SOURCE.get(start..end).ok_or("admitted package range")?;
    assert!(selected_bytes.starts_with("def _inner"));
    assert_eq!(query::literal(projected, "source")?, selected_bytes);
    let view = serde_json::from_value(answer["view"]["id"].clone())?;
    let original = serde_json::from_value(answer["answer"].clone())?;
    let replay = rift_search::sparql::continuation_query(&original, &view, 0)?;
    std::fs::write(root.join(SOURCE_PATH), SOURCE.replace("_inner", "_changed"))?;
    let held = proxied_call(&client, "search", &json!({"query":replay})).await?;
    assert_eq!(held["answer"], answer["answer"]);
    assert_eq!(held["view"], answer["view"]);
    let changed = proxied_call(&client, "search", &json!({"query":statement})).await?;
    assert_eq!(row(&changed)?["name"]["value"], "_changed");
    client.cancel().await?;
    require_success(
        &run_rift(root, &["server", "stop"]).await?,
        "restart package fixture",
    )?;
    std::fs::remove_file(root.join(RECORD_PATH))?;
    assert!(root.join(SOURCE_PATH).is_file(), "source remains installed");
    let client = proxy_client(root).await?;
    await_workspace_ready(&client).await?;
    let expired = proxied_call(&client, "search", &json!({"query":replay})).await?;
    assert_eq!(expired["outcome"], "unavailable", "{expired}");
    assert_eq!(expired["reason"], "view_expired", "{expired}");
    let unadmitted = proxied_call(&client, "search", &json!({"query":statement})).await?;
    assert_eq!(unadmitted["outcome"], "unavailable", "{unadmitted}");
    assert_eq!(
        unadmitted["reason"], "insufficient_coverage",
        "{unadmitted}"
    );
    assert!(
        !fixture
            .requests()
            .await
            .iter()
            .any(|request| request.uri.ends_with("/declarations"))
    );
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}
