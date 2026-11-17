//! Local declarations answer symbol reads with their original source.

use crate::workspace_client::{
    TestResult, call_retrying_acceptance, captured_seed, served_workspace, tool_request,
};
use serde_json::json;

#[tokio::test]
async fn local_symbol_ids_answer_reads_for_each_language() -> TestResult {
    let cases = [
        (
            "main.ts",
            "export function typescript_beacon(): number { return 1; }",
            "typescript_beacon",
        ),
        (
            "main.tsx",
            "export function tsx_beacon() { return <main />; }",
            "tsx_beacon",
        ),
        (
            "main.js",
            "export function javascript_beacon() { return 1; }",
            "javascript_beacon",
        ),
        (
            "src/pkg/main.py",
            "def python_beacon():\n    return 1\n",
            "python_beacon",
        ),
        (
            "README.md",
            "# Markdown beacon\n\nText.\n",
            "Markdown beacon",
        ),
        ("settings.json", "{\"json_beacon\": 1}", "json_beacon"),
        ("settings.yaml", "yaml_beacon: 1\n", "yaml_beacon"),
        ("settings.toml", "toml_beacon = 1\n", "toml_beacon"),
        ("src/lib.rs", "pub fn rust_beacon() {}", "rust_beacon"),
        (
            "pkg/bad.py",
            "def valid_beacon():\n    pass\ndef broken(: pass\n",
            "valid_beacon",
        ),
    ];
    let mut files = cases
        .iter()
        .map(|(path, text, _)| (*path, *text))
        .collect::<Vec<_>>();
    files.push(("Cargo.toml", "[package]\nname='beacon'\nversion='1.0.0'\n"));
    files.push((
        "pyproject.toml",
        "[build-system]\nrequires=['hatchling']\nbuild-backend='hatchling.build'\n",
    ));
    let (_directory, client, task) = served_workspace(&files, None).await?;
    for (path, source, name) in cases {
        let id = captured_seed(&client, name, path).await?;
        let symbol =
            call_retrying_acceptance(&client, tool_request("get_symbol", &json!({"id": id})))
                .await?;
        assert_eq!(symbol["symbol"]["name"], name, "{symbol}");
        assert_eq!(symbol["symbol"]["id"], id, "{symbol}");
        let facets =
            call_retrying_acceptance(&client, tool_request("nodes", &json!({"symbol": id})))
                .await?;
        assert_eq!(facets["symbol"], id, "{facets}");
        let declarations = facets["declarations"].as_array().ok_or("declarations")?;
        assert_eq!(declarations.len(), 1, "{facets}");
        let full = call_retrying_acceptance(
            &client,
            tool_request("nodes", &json!({"symbol": id, "syntax": true})),
        )
        .await?;
        let elements = full["nodes"].as_array().ok_or("syntax elements")?;
        assert!(
            elements.iter().any(|element| element["source"]
                .as_str()
                .is_some_and(|text| source.contains(text) && text.contains(name))),
            "{full}"
        );
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}
