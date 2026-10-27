//! Additional source languages reach workspace reads with original source bytes.

mod hermetic_search;
#[expect(
    dead_code,
    reason = "This suite uses the relative-root workspace fixture."
)]
mod workspace_client;

use rift_protocol::error::ErrorCode;
use serde_json::{Value, json};
use workspace_client::query;
use workspace_client::{
    TestResult, call_retrying_acceptance, failed_call, search_after_population,
    served_relative_workspace, served_workspace, tool_request,
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
                &json!({"query":"SELECT ?node { ?node a :Node; :unit <rift://source/project/unknown.beacon> }"}),
            ))
            .await,
    )?;
    assert_eq!(failure.code, ErrorCode::TemporarilyUnavailable);
    assert!(
        failure.message.contains("required Nodes facts"),
        "{}",
        failure.message
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

async fn assert_workspace_nodes(client: &Client, case: &SourceCase) -> TestResult {
    let unit = rift_core::SourceUnitId::new(
        rift_core::SourceResolverId::new("project")?,
        rift_core::SourcePath::new(case.path)?,
    )?;
    let output = call_retrying_acceptance(
        client,
        tool_request("nodes", &json!({"query":format!(
            "SELECT ?node ?id ?language ?start ?end ?text {{ ?node a :Node; :unit <{unit}>; :id ?id; :language ?language; :range [ :start ?start; :end ?end ] . <{unit}> :source ?text }} ORDER BY ?start ?end"
        )})),
    ).await?;
    let nodes = query::rows(&output)?;
    assert!(!nodes.is_empty(), "{}: {output}", case.path);
    for node in nodes {
        assert_eq!(
            query::literal(node, "language")?,
            case.language,
            "{}: {output}",
            case.path
        );
        assert_eq!(
            query::literal(node, "text")?,
            case.text,
            "{}: original source",
            case.path
        );
        assert!(!query::literal(node, "id")?.is_empty());
        let start = query::literal(node, "start")?.parse::<usize>()?;
        let end = query::literal(node, "end")?.parse::<usize>()?;
        assert!(
            case.text.get(start..end).is_some(),
            "original UTF-8 node range: {node}"
        );
    }
    Ok(())
}

async fn assert_workspace_search(client: &Client, case: &SourceCase) -> TestResult {
    let output = search_after_population(
        client,
        &workspace_client::query::retrieval(&serde_json::from_value(json!({
            "pattern":"灯", "target":"file", "paths":{"include":[case.path]},
            "include":["source"],
        }))?)?,
    )
    .await?;
    let hits = workspace_client::query::rows(&output)?;
    let hit = hits
        .iter()
        .find(|hit| workspace_client::query::literal(hit, "path").ok() == Some(case.path))
        .ok_or_else(|| format!("{} missing from search: {output}", case.path))?;
    assert!(workspace_client::query::literal(hit, "source")?.contains('灯'));
    Ok(())
}

async fn assert_workspace_symbol(client: &Client, case: &SourceCase, name: &str) -> TestResult {
    let output = search_after_population(
        client,
        &workspace_client::query::retrieval(&serde_json::from_value(json!({
            "query":name, "target":"symbol", "paths":{"include":[case.path]},
            "include":["source"],
        }))?)?,
    )
    .await?;
    let results = workspace_client::query::rows(&output)?;
    let matching = results
        .iter()
        .filter(|hit| {
            workspace_client::query::literal(hit, "path").ok() == Some(case.path)
                && workspace_client::query::literal(hit, "name").ok() == Some(name)
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "{name} physical declaration: {output}");
    let hit = matching[0];
    assert!(
        hit["symbol"]["type"] == "blank_node",
        "fixture has no captured namespace context: {hit}"
    );
    assert_eq!(
        workspace_client::query::literal(hit, "language")?,
        case.language,
        "{hit}"
    );
    if case.path == "default.h" {
        assert_eq!(
            workspace_client::query::literal(hit, "kind")?,
            "function",
            "C function prototype: {hit}"
        );
    }
    let start = workspace_client::query::literal(hit, "start")?.parse::<usize>()?;
    let end = workspace_client::query::literal(hit, "end")?.parse::<usize>()?;
    assert_eq!(
        workspace_client::query::literal(hit, "source")?,
        &case.text[start..end]
    );
    assert_workspace_symbol_node(client, case, name, start, end).await?;
    let search = search_after_population(
        client,
        &workspace_client::query::retrieval(&serde_json::from_value(json!({
            "query":name, "target":"symbol", "paths":{"include":[case.path]},
        }))?)?,
    )
    .await?;
    let results = workspace_client::query::rows(&search)?;
    assert!(
        results.iter().any(|result| {
            workspace_client::query::literal(result, "path").ok() == Some(case.path)
                && workspace_client::query::literal(result, "name").ok() == Some(name)
                && result["symbol"]["type"] == "blank_node"
                && result["start"] == hit["start"]
                && result["end"] == hit["end"]
        }),
        "{name} declaration missing from search: {search}"
    );
    Ok(())
}

async fn assert_workspace_symbol_node(
    client: &Client,
    case: &SourceCase,
    name: &str,
    start: usize,
    end: usize,
) -> TestResult {
    let unit = rift_core::SourceUnitId::new(
        rift_core::SourceResolverId::new("project")?,
        rift_core::SourcePath::new(case.path)?,
    )?
    .to_string();
    let source = &case.text[start..end];
    let positions = source.match_indices(name).collect::<Vec<_>>();
    assert_eq!(positions.len(), 1, "{name} has one original name span");
    let position = u64::try_from(start.checked_add(positions[0].0).ok_or("name offset")?)?;
    let output = call_retrying_acceptance(
        client,
        tool_request("nodes", &json!({"query":format!(
            "SELECT ?node ?id ?language ?node_start ?node_end ?text {{ ?node a :Node; :unit <{unit}>; :id ?id; :language ?language; :range [ :start ?node_start; :end ?node_end ] . <{unit}> :source ?text }} ORDER BY ?node_start ?node_end"
        )})),
    ).await?;
    let _: jiff::Timestamp = output["view"]["expires_at"]
        .as_str()
        .ok_or("node query expiry")?
        .parse()?;
    let nodes = query::rows(&output)?;
    let start = u64::try_from(start)?;
    let end = u64::try_from(end)?;
    let node = nodes
        .iter()
        .filter(|node| {
            query::literal(node, "node_start")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|node_start| node_start >= start && node_start <= position)
                && query::literal(node, "node_end")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    == Some(end)
        })
        .max_by_key(|node| {
            query::literal(node, "node_start")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
        })
        .ok_or_else(|| format!("{name} declaration node: {output}"))?;
    assert!(
        !query::literal(node, "id")?.is_empty(),
        "source carries node address: {node}"
    );
    assert_eq!(query::literal(node, "language")?, case.language);
    let node_start = query::literal(node, "node_start")?.parse::<usize>()?;
    let node_end = usize::try_from(end)?;
    let actual = query::literal(node, "text")?;
    assert_eq!(actual, case.text);
    let expected = case
        .text
        .get(node_start..node_end)
        .ok_or("original node byte range")?;
    assert_node_source_span(
        client,
        &rift_protocol::source_read::GetSourceParams {
            unit: rift_protocol::read::SourceUnitId::parse(&unit)?,
            range: Some(rift_protocol::read::TextRange {
                start: u64::try_from(node_start)?,
                end: u64::try_from(node_end)?,
            }),
            view: Some(serde_json::from_value(output["view"]["id"].clone())?),
            rev: None,
        },
        expected,
        &output["view"],
    )
    .await?;
    Ok(())
}

async fn assert_node_source_span(
    client: &Client,
    request: &rift_protocol::source_read::GetSourceParams,
    expected: &str,
    view: &Value,
) -> TestResult {
    let answer = call_retrying_acceptance(
        client,
        tool_request("search", &serde_json::to_value(query::source(request)?)?),
    )
    .await?;
    let [row] = query::rows(&answer)? else {
        return Err(format!("one original node source range: {answer}").into());
    };
    assert_eq!(query::literal(row, "outcome")?, "found");
    assert_eq!(query::literal(row, "complete")?, "true");
    assert_eq!(query::iri(row, "unit")?, request.unit.as_str());
    let range = request.range.as_ref().ok_or("original node range")?;
    assert_eq!(query::literal(row, "start")?.parse::<u64>()?, range.start);
    assert_eq!(query::literal(row, "end")?.parse::<u64>()?, range.end);
    assert_eq!(query::literal(row, "text")?, expected);
    assert_eq!(
        query::literal(row, "source_view")?,
        view["id"].as_str().ok_or("original node source view")?
    );
    assert_eq!(
        query::literal(row, "source_expiry")?,
        view["expires_at"]
            .as_str()
            .ok_or("original node source expiry")?
    );
    Ok(())
}

#[tokio::test]
async fn published_symbols_without_canonical_identities_remain_queryable() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(
        &[(
            "client.c",
            "int a_other(void) { return 2; }\nint c_beacon(void) { return 1; }\n",
        )],
        None,
    )
    .await?;
    let answer = call_retrying_acceptance(
        &client,
        tool_request("search", &serde_json::json!({"query":"SELECT ?symbol { ?symbol a :Symbol; :name \"c_beacon\" }"})),
    ).await?;
    let rows = workspace_client::query::rows(&answer)?;
    assert_eq!(rows.len(), 1, "{answer}");
    assert_eq!(rows[0]["symbol"]["type"], "blank_node", "{answer}");
    let generic = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"query":"SELECT ?symbol { ?symbol a :Symbol; :name ?name . FILTER(?name = \"c_beacon\") }"})),
    ).await?;
    assert_eq!(workspace_client::query::rows(&generic)?, rows, "{generic}");
    let source = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"query":"SELECT ?source { ?symbol a :Symbol; :name \"c_beacon\"; :declarations ?declaration . ?declaration :source ?source }"})),
    ).await?;
    let source_rows = workspace_client::query::rows(&source)?;
    assert_eq!(source_rows.len(), 1, "{source}");
    assert!(
        workspace_client::query::literal(&source_rows[0], "source")?
            .contains("int c_beacon(void) { return 1; }"),
        "{source}"
    );
    client.cancel().await?;
    Ok(())
}
