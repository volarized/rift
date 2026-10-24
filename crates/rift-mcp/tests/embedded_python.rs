//! Incoming Python references through the embedded ty engine and public MCP tools.

mod hermetic_search;
#[expect(
    dead_code,
    reason = "This suite uses the relative-root workspace fixture."
)]
mod workspace_client;

use workspace_client::{
    TestResult, call_retrying_acceptance, served_relative_workspace, tool_request,
};

const SERVICE: &str = "def serve(port: int) -> int:\n    return port\n";
const CALLER: &str = "from service import serve\n\ndef caller() -> int:\n    return serve(8080)\n";
const PYPROJECT: &str = "[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['service','main']\n";
const CONFIGURATION: &str = "\
[languages.python.lsp]\nembedded = \"ty\"\n\
retry = { attempts = 2, delay = \"1ms\", delay_limit = \"1ms\" }\n";

#[tokio::test]
async fn incoming_references_resolve_cross_file_callers_without_writes() -> TestResult {
    let (directory, client, server_task) = served_relative_workspace(
        &[
            ("service.py", SERVICE),
            ("main.py", CALLER),
            ("pyproject.toml", PYPROJECT),
        ],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let seed = workspace_client::captured_seed(&client, "serve", "service.py").await?;
    let answer = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &serde_json::to_value(workspace_client::query::incoming_references(&seed)?)?,
        ),
    )
    .await?;
    let results = workspace_client::query::rows(&answer)?;
    assert_eq!(results.len(), 1, "{answer:#}");
    let hit = &results[0];
    assert_eq!(
        workspace_client::query::literal(hit, "path")?,
        "main.py",
        "{answer:#}"
    );
    assert_eq!(
        workspace_client::query::literal(hit, "name")?,
        "caller",
        "{answer:#}"
    );
    assert_eq!(
        workspace_client::query::iri(hit, "facet")?,
        "urn:rift:vocabulary:references",
        "{answer:#}"
    );
    assert_eq!(
        workspace_client::query::iri(hit, "derivation")?,
        "urn:rift:vocabulary:resolution",
        "{answer:#}"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("service.py"))?,
        SERVICE
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("main.py"))?,
        CALLER
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}
