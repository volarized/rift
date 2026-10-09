//! Additional source languages reach workspace reads with original source bytes.

mod hermetic_search;
#[expect(
    dead_code,
    reason = "This suite uses the relative-root workspace fixture."
)]
mod workspace_client;

use rift_protocol::error::ErrorCode;
use serde_json::{Value, json};
use workspace_client::{
    TestResult, call_retrying_acceptance, failed_call, search_after_population,
    served_relative_workspace, tool_request,
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

struct SourceCase {
    path: &'static str,
    text: &'static str,
    language: &'static str,
    symbol: Option<&'static str>,
}

const SOURCE_CASES: &[SourceCase] = &[
    SourceCase {
        path: "plain.html",
        text: "<!-- 灯 --><main><script>function html_beacon() { return 1; }</script></main>",
        language: "html",
        symbol: Some("html_beacon"),
    },
    SourceCase {
        path: "plain.css",
        text: "/* 灯 */ .css_beacon { --color: blue; color: var(--color); }",
        language: "css",
        symbol: Some(".css_beacon"),
    },
    SourceCase {
        path: "panel.vue",
        text: "<template><main>灯</main></template><script lang=\"ts\">export function vue_beacon(): number { return 1; }</script>",
        language: "vue",
        symbol: Some("vue_beacon"),
    },
    SourceCase {
        path: "panel.svelte",
        text: "<script lang=\"ts\">export function svelte_beacon(): number { return 1; }</script><main>灯</main>",
        language: "svelte",
        symbol: Some("svelte_beacon"),
    },
    SourceCase {
        path: "client.c",
        text: "/* 灯 */ int c_beacon(void) { return 1; }",
        language: "c",
        symbol: Some("c_beacon"),
    },
    SourceCase {
        path: "default.h",
        text: "/* 灯 */ int header_beacon(void);",
        language: "c",
        symbol: Some("header_beacon"),
    },
    SourceCase {
        path: "client.cpp",
        text: "// 灯\nnamespace client { class CppBeacon {}; }",
        language: "cpp",
        symbol: Some("CppBeacon"),
    },
    SourceCase {
        path: "client.hpp",
        text: "// 灯\nclass HeaderBeacon {};",
        language: "cpp",
        symbol: Some("HeaderBeacon"),
    },
    SourceCase {
        path: "override.h",
        text: "// 灯\nnamespace client { class OverrideBeacon {}; }",
        language: "cpp",
        symbol: Some("OverrideBeacon"),
    },
    SourceCase {
        path: "client.pyx",
        text: "# 灯\ninclude \"base.pxi\"\ncdef class CythonBeacon:\n    cpdef int open(self):\n        return 1\n",
        language: "cython",
        symbol: Some("CythonBeacon"),
    },
    SourceCase {
        path: "client.pxd",
        text: "# 灯\ncdef int cython_beacon(int port)\n",
        language: "cython",
        symbol: Some("cython_beacon"),
    },
    SourceCase {
        path: "base.pxi",
        text: "# 灯\ncdef int cython_port = 8080\n",
        language: "cython",
        symbol: Some("cython_port"),
    },
    SourceCase {
        path: "settings.jsonc",
        text: "// 灯 beacon\n{\"port\":8080}",
        language: "jsonc",
        symbol: None,
    },
    SourceCase {
        path: "component.html",
        text: "@if (ready) { <main>灯 {{ angular_beacon | uppercase }}</main> }",
        language: "html:angular",
        symbol: None,
    },
];

const CONFIGURATION: &str = "\
[languages.c]\nexclude = [\"override.h\"]\n\
[languages.cpp]\ninclude = [\"**/*.cpp\", \"**/*.hpp\", \"override.h\"]\n\
[languages.html]\nexclude = [\"component.html\"]\n\
[languages.\"html:angular\"]\ninclude = [\"component.html\"]\n";

#[tokio::test]
async fn workspace_discovers_native_web_and_explicit_template_languages() -> TestResult {
    let mut files: Vec<_> = SOURCE_CASES
        .iter()
        .map(|case| (case.path, case.text))
        .collect();
    files.push(("unknown.beacon", "unsupported_beacon 灯"));
    let (_directory, client, server_task) =
        served_relative_workspace(&files, Some(CONFIGURATION.to_owned())).await?;
    for case in SOURCE_CASES {
        assert_workspace_nodes(&client, case).await?;
        assert_workspace_search(&client, case).await?;
        if let Some(name) = case.symbol {
            assert_workspace_symbol(&client, case, name).await?;
        }
    }
    let failure = failed_call(
        client
            .call_tool(tool_request(
                "nodes",
                &json!({"path":"unknown.beacon", "position":0}),
            ))
            .await,
    )?;
    assert_eq!(failure.code, ErrorCode::CapabilityUnavailable);
    assert!(failure.message.contains("beacon"), "{}", failure.message);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

async fn assert_workspace_nodes(client: &Client, case: &SourceCase) -> TestResult {
    let output = call_retrying_acceptance(
        client,
        tool_request("nodes", &json!({"path":case.path, "position":0})),
    )
    .await?;
    let nodes = output["nodes"].as_array().ok_or("nodes array")?;
    assert!(!nodes.is_empty(), "{}: {output}", case.path);
    assert_eq!(
        nodes[0]["language"], case.language,
        "{}: {output}",
        case.path
    );
    assert_eq!(
        output["source"][0], case.text,
        "{}: original source",
        case.path
    );
    Ok(())
}

async fn assert_workspace_search(client: &Client, case: &SourceCase) -> TestResult {
    let output = search_after_population(
        client,
        &json!({
            "pattern":"灯", "target":"file", "paths":{"include":[case.path]},
            "include":["source"],
        }),
    )
    .await?;
    let hits = output["results"].as_array().ok_or("search results")?;
    let hit = hits
        .iter()
        .find(|hit| hit["path"] == case.path)
        .ok_or_else(|| format!("{} missing from search: {output}", case.path))?;
    assert!(
        hit["source"]
            .as_str()
            .is_some_and(|source| source.contains('灯'))
    );
    Ok(())
}

async fn assert_workspace_symbol(client: &Client, case: &SourceCase, name: &str) -> TestResult {
    let output = call_retrying_acceptance(
        client,
        tool_request("get_symbol", &json!({"name":name, "include":["source"]})),
    )
    .await?;
    let hits = output["hits"].as_array().ok_or("symbol hits")?;
    let hit = hits
        .iter()
        .find(|hit| hit["path"] == case.path && hit["symbol"]["name"] == name)
        .ok_or_else(|| format!("{name} missing from {}: {output}", case.path))?;
    assert_eq!(hit["symbol"]["language"], case.language, "{hit}");
    if case.path == "default.h" {
        assert_eq!(
            hit["symbol"]["kind"], "function",
            "C function prototype: {hit}"
        );
    }
    let start = usize::try_from(hit["range"]["start"].as_u64().ok_or("range start")?)?;
    let end = usize::try_from(hit["range"]["end"].as_u64().ok_or("range end")?)?;
    assert_eq!(hit["source"], Value::from(&case.text[start..end]));
    assert!(
        hit["node"].is_string(),
        "source carries node address: {hit}"
    );
    let search = search_after_population(
        client,
        &json!({
            "query":name, "target":"symbol", "paths":{"include":[case.path]},
        }),
    )
    .await?;
    let results = search["results"]
        .as_array()
        .ok_or("symbol search results")?;
    assert!(
        results.iter().any(|result| {
            result["path"] == case.path && result["hit"]["symbol"]["id"] == hit["symbol"]["id"]
        }),
        "{name} declaration missing from search: {search}"
    );
    Ok(())
}
