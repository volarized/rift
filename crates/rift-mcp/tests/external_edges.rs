//! Static edges to dependency and standard-library symbols through the public MCP client.

#[expect(
    dead_code,
    reason = "shared workspace helpers serve several integration suites"
)]
mod workspace_client;

mod hermetic_search;

use std::collections::BTreeSet;

use rift_protocol::query::QueryParams;
use serde_json::{Value, json};
use workspace_client::{TestResult, captured_seed, query_after_population, served_workspace};

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn answer(client: &Client, query: &str) -> TestResult<Value> {
    let params = QueryParams {
        query: query.to_owned(),
        max_results: 100,
        ..QueryParams::default()
    };
    query_after_population(client, &params).await
}

/// The `?to` column of one answered query.
fn targets(answer: &Value) -> TestResult<BTreeSet<String>> {
    workspace_client::query::rows(answer)?
        .iter()
        .map(|row| Ok(workspace_client::query::iri(row, "to")?.to_owned()))
        .collect()
}

/// Whether a warning states that edges of `kind` may be missing.
fn warns_of(answer: &Value, kind: &str) -> bool {
    answer["warnings"].as_array().is_some_and(|warnings| {
        warnings.iter().any(|warning| {
            warning["code"] == "edge_coverage_missing"
                && warning["kinds"]
                    .as_array()
                    .is_some_and(|kinds| kinds.contains(&json!(kind)))
        })
    })
}

/// Requires `seed` to reach exactly `expected` over `predicate`, as a seeded walk and as
/// rows of the whole inventory.
async fn assert_edges(
    client: &Client,
    seed: &str,
    predicate: &str,
    expected: &[&str],
) -> TestResult {
    let expected = expected
        .iter()
        .map(|target| (*target).to_owned())
        .collect::<BTreeSet<_>>();
    let walk = answer(
        client,
        &format!("SELECT ?to {{ <{seed}> :{predicate} ?to }}"),
    )
    .await?;
    assert_eq!(
        targets(&walk)?,
        expected,
        "walk {seed} :{predicate}: {walk}"
    );
    let inventory = answer(
        client,
        &format!("SELECT ?to {{ ?from :{predicate} ?to FILTER(?from = <{seed}>) }}"),
    )
    .await?;
    assert_eq!(
        targets(&inventory)?,
        expected,
        "inventory {seed} :{predicate}: {inventory}"
    );
    Ok(())
}

#[tokio::test]
async fn test_rust_edges_name_crates_and_the_standard_library() -> TestResult {
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nserde_json = \"1\"\n",
        ),
        (
            "src/lib.rs",
            "pub mod globbed;\npub mod shadow { pub struct Display; }\npub struct Task;\n\
             impl std::fmt::Display for Task {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(\"\") }\n}\n\
             impl undeclared::Trait for Task {}\n\
             pub fn encode() -> String { serde_json::to_string(&1).unwrap_or_default() }\n\
             pub fn build() { let built: Vec<u8> = Vec::new(); }\n",
        ),
        (
            "src/globbed.rs",
            "use serde_json::*;\npub fn under() { to_string(&1); }\n",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    captured_seed(&client, "encode", "src/lib.rs").await?;
    let display = "rift://symbol/stdlib/rust/rust/std/fmt/Display";
    let to_string = "rift://symbol/cargo/crates.io/serde_json/rust/serde_json/to_string";
    for (seed, predicate, expected) in [
        ("fixture/Task", "implements", display),
        ("fixture/encode", "calls", to_string),
        (
            "fixture/build",
            "calls",
            "rift://symbol/stdlib/rust/rust/alloc/vec/Vec/new",
        ),
        (
            "fixture/globbed",
            "imports",
            "rift://symbol/cargo/crates.io/serde_json/rust/serde_json",
        ),
    ] {
        let seed = format!("rift://symbol/local/rust/{seed}");
        assert_edges(&client, &seed, predicate, &[expected]).await?;
    }
    let inventory = answer(&client, "SELECT ?to { ?from :implements ?to }").await?;
    assert_eq!(targets(&inventory)?, BTreeSet::from([display.to_owned()]));
    assert!(warns_of(&inventory, "implements"), "{inventory}");
    // The name under the glob import stays without a target and is counted.
    let calls = answer(&client, "SELECT ?to { ?from :calls ?to }").await?;
    assert!(warns_of(&calls, "calls"), "{calls}");
    let under = answer(
        &client,
        "ASK { <rift://symbol/local/rust/fixture/globbed/under> :calls ?to }",
    )
    .await?;
    assert_eq!(under["reason"], "insufficient_coverage", "{under}");
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_python_edges_name_distributions_and_builtins() -> TestResult {
    let files = [
        (
            "pyproject.toml",
            "[project]\nname = 'app'\nversion = '0.1.0'\ndependencies = ['greeting==1.0.0']\n[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['app']\n",
        ),
        (
            "app.py",
            "import os.path\nfrom greeting.core import greet, Base\n\nclass Child(Base):\n    pass\n\n\
             def encode(items: Base):\n    greet()\n    count = len(items)\n    return os.path.join('a', 'b')\n",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    captured_seed(&client, "encode", "app.py").await?;
    let greet = "rift://symbol/pypi/pypi.org/greeting/python/greeting/core/greet";
    let base = "rift://symbol/pypi/pypi.org/greeting/python/greeting/core/Base";
    let join = "rift://symbol/stdlib/cpython/python/os/path/join";
    let cases: [(&str, &str, &[&str]); 4] = [
        ("app/Child", "extends", &[base]),
        (
            "app/encode",
            "calls",
            &[
                greet,
                join,
                "rift://symbol/stdlib/cpython/python/builtins/len",
            ],
        ),
        ("app/encode", "hasType", &[base]),
        (
            "app/encode",
            "reads",
            &[join, "rift://symbol/stdlib/cpython/python/os/path"],
        ),
    ];
    for (seed, predicate, expected) in cases {
        let seed = format!("rift://symbol/local/python/{seed}");
        assert_edges(&client, &seed, predicate, expected).await?;
    }
    assert_edges(
        &client,
        "rift://symbol/local/python/app",
        "imports",
        &[greet, base, "rift://symbol/stdlib/cpython/python/os/path"],
    )
    .await?;
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_typescript_edges_name_packages_and_node_modules() -> TestResult {
    let files = [
        (
            "package.json",
            "{\"name\":\"app\",\"version\":\"1.0.0\",\"dependencies\":{\"zod\":\"4.6.7\"}}",
        ),
        (
            "src/app.ts",
            "import { z, ZodType } from \"zod\";\nimport * as all from \"zod\";\nimport { readFileSync } from \"node:fs\";\n\
             export { ZodError as Failure } from \"zod\";\nexport { gone } from \"undeclared\";\n\
             export class Shape implements ZodType {}\n\
             export function encode(value: ZodType) { z.string(); all.parse(1); readFileSync(\"a\"); }\n",
        ),
        (
            "src/plain.mjs",
            "import { readFileSync } from \"fs\";\nexport function load() { readFileSync(\"a\"); }\n",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    captured_seed(&client, "encode", "src/app.ts").await?;
    let zod = "rift://symbol/npm/npmjs.org/zod/typescript/zod";
    let read_file = "rift://symbol/stdlib/node/javascript/fs/readFileSync";
    let types = format!("{zod}/ZodType");
    let cases: [(&str, &str, &[&str]); 4] = [
        ("typescript/.%2Fsrc%2Fapp.ts/Shape", "implements", &[&types]),
        (
            "typescript/.%2Fsrc%2Fapp.ts/encode",
            "calls",
            &[
                &format!("{zod}/z/string"),
                &format!("{zod}/parse"),
                read_file,
            ],
        ),
        ("javascript/.%2Fsrc%2Fplain.mjs/load", "calls", &[read_file]),
        (
            "typescript/.%2Fsrc%2Fapp.ts",
            "imports",
            &[
                zod,
                &format!("{zod}/z"),
                &types,
                &format!("{zod}/ZodError"),
                read_file,
            ],
        ),
    ];
    for (seed, predicate, expected) in cases {
        let seed = format!("rift://symbol/local/{seed}");
        assert_edges(&client, &seed, predicate, expected).await?;
    }
    let imports = answer(&client, "SELECT ?to { ?from :imports ?to }").await?;
    assert!(warns_of(&imports, "imports"), "{imports}");
    client.cancel().await?;
    task.await?;
    Ok(())
}
