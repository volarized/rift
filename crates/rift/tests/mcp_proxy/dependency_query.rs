//! Dependency call paths and captured package reads through the real stdio proxy.

use serde_json::{Value, json};

use super::global_api::{FixtureOptions, GlobalFixture};
use super::harness::{
    FailureWindow, StopOnDrop, TestResult, await_workspace_ready, laid_out_workspace, proxied_call,
    proxy_client,
};

const SOURCE: &str = "def greet(name: str) -> str:\n    return _helper(name)\n\ndef _helper(name: str) -> str:\n    return _inner(name)\n\ndef _inner(name: str) -> str:\n    return name\n";
const SOURCE_PATH: &str = ".venv/lib/python3.12/site-packages/greeting/core.py";
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
        ".venv/lib/python3.12/site-packages/greeting-1.0.0.dist-info/RECORD",
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
        "SELECT ?symbol ?name {{ <{seed}> :calls/:calls/:calls ?symbol . ?symbol :name ?name }}"
    );
    let answer = proxied_call(&client, "search", &json!({"query":statement})).await?;
    let projected = row(&answer)?;
    assert_eq!(projected["name"]["value"], "_inner");
    let identity = projected["symbol"]["value"]
        .as_str()
        .ok_or("canonical package declaration")?;
    let parsed = rift_protocol::identity::SymbolIdentity::parse(identity)?;
    assert!(
        matches!(parsed.owner(), rift_protocol::identity::SymbolOwner::Package { manager, registry, name, version } if manager == "pypi" && registry == "pypi.org" && name == "greeting" && version == "1.0.0")
    );
    let selected = &answer["view"];
    let exact = proxied_call(
        &client,
        "get_symbol",
        &json!({"id":identity,"view":selected["id"],"include":["source"]}),
    )
    .await?;
    assert_eq!(exact["symbol"]["name"], "_inner");
    assert_eq!(exact["view"], *selected);
    let unit = exact["declarations"][0]["unit"]
        .as_str()
        .ok_or("original package unit")?;
    std::fs::write(root.join(SOURCE_PATH), SOURCE.replace("_inner", "_changed"))?;
    let held = proxied_call(
        &client,
        "get_source",
        &json!({"unit":unit,"view":selected["id"]}),
    )
    .await?;
    assert_eq!(held["source"]["text"], SOURCE);
    assert_eq!(held["view"], *selected);
    let exact_held = proxied_call(
        &client,
        "get_symbol",
        &json!({"id":identity,"view":selected["id"],"include":["source"]}),
    )
    .await?;
    assert_eq!(exact_held, exact);
    let retained = format!(
        "SELECT ?symbol ?name {{ SERVICE <{}{}> {{}} }}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        selected["id"].as_str().ok_or("captured query view")?
    );
    let retained = proxied_call(&client, "search", &json!({"query":retained})).await?;
    assert_eq!(retained["answer"], answer["answer"]);
    assert_eq!(retained["view"]["expires_at"], selected["expires_at"]);
    let changed = proxied_call(&client, "search", &json!({"query":statement})).await?;
    assert_eq!(row(&changed)?["name"]["value"], "_changed");
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
