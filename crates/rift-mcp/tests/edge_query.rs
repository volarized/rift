//! Edge queries through the public MCP client.

#[expect(
    dead_code,
    reason = "shared workspace helpers serve several integration suites"
)]
mod workspace_client;

mod hermetic_search;

use rift_protocol::query::{QueryLanguage, QueryParams};
use serde_json::json;
use workspace_client::{TestResult, captured_seed, query_after_population, served_workspace};

#[tokio::test]
async fn test_edge_static_calls_agree_in_both_directions_and_with_flows() -> TestResult {
    let source = "export function root() { leaf(); }\nfunction leaf() { return 'zebraquartz'; }\n";
    let (_directory, client, task) = served_workspace(&[("entry.mjs", source)], None).await?;
    let root = captured_seed(&client, "root", "entry.mjs").await?;
    let leaf = captured_seed(&client, "leaf", "entry.mjs").await?;
    for text in [
        format!("ASK {{ <{root}> :calls ?target . FILTER(?target = <{leaf}>) }}"),
        format!("ASK {{ ?caller :calls <{leaf}> . FILTER(?caller = <{root}>) }}"),
        format!(
            "ASK {{ SERVICE :flows {{ ?flow :query 'zebraquartz'; :max_results 1; :steps ?step . ?step :value <{root}> }} }}"
        ),
    ] {
        let params = QueryParams {
            query: text,
            language: QueryLanguage::Sparql,
            page: 1,
            max_results: 10,
            revision: None,
        };
        let answer = query_after_population(&client, &params).await?;
        assert_eq!(
            answer["answer"],
            json!({"result":"boolean", "value":true}),
            "{answer}"
        );
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_edge_heritage_imports_aliases_and_outgoing_references_are_queryable() -> TestResult {
    let files = [
        (
            "entry.mjs",
            "import { Base } from './base.mjs';\nexport class Derived extends Base {}\nexport { Derived as Public };\n",
        ),
        ("base.mjs", "export class Base {}\n"),
    ];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    let derived = captured_seed(&client, "Derived", "entry.mjs").await?;
    let base = captured_seed(&client, "Base", "base.mjs").await?;
    for text in [
        format!("ASK {{ <{derived}> :extends <{base}> }}"),
        format!("ASK {{ ?module :imports <{base}> }}"),
        format!("ASK {{ ?alias :aliases <{derived}> }}"),
        format!("ASK {{ <{derived}> :references <{base}> }}"),
        "ASK { ?module :imports ?target }".to_owned(),
        "ASK { ?alias :aliases ?target }".to_owned(),
    ] {
        let params = QueryParams {
            query: text,
            language: QueryLanguage::Sparql,
            page: 1,
            max_results: 10,
            revision: None,
        };
        let answer = query_after_population(&client, &params).await?;
        assert_eq!(
            answer["answer"],
            json!({"result":"boolean", "value":true}),
            "{answer}"
        );
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[cfg(unix)]
const EDGE_ANALYZING_ENGINE: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"callHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"edge/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"id":'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;

#[cfg(unix)]
#[tokio::test]
async fn test_edge_readiness_budget_reports_a_typed_warning_for_calls_and_struct_usages()
-> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("edges.sh");
    std::fs::write(&script, EDGE_ANALYZING_ENGINE)?;
    let command = serde_json::to_string(&script)?;
    let configuration = format!(
        "[server]\nreadiness_timeout='1s'\n[languages.rust.lsp]\ncommand=['sh',{command}]\n"
    );
    let files = [
        (
            "Cargo.toml",
            "[package]\nname='edge_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n",
        ),
        (
            "lib.rs",
            "pub struct Worker;\npub fn root(callback: fn()) { callback(); }\n",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, Some(configuration)).await?;
    let root = captured_seed(&client, "root", "lib.rs").await?;
    let worker = captured_seed(&client, "Worker", "lib.rs").await?;
    for text in [
        format!("SELECT ?target {{ <{root}> :calls ?target }}"),
        format!("SELECT ?usage {{ <{worker}> :usages ?usage }}"),
    ] {
        let params = QueryParams {
            query: text,
            language: QueryLanguage::Sparql,
            page: 1,
            max_results: 10,
            revision: None,
        };
        let answer = query_after_population(&client, &params).await?;
        let warnings = answer["warnings"]
            .as_array()
            .ok_or_else(|| format!("typed readiness warning: {answer}"))?;
        assert!(
            warnings
                .iter()
                .any(|warning| warning["code"] == "engine_analysis_unavailable"
                    && warning["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("readiness_timeout"))),
            "{answer}"
        );
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[path = "../../rift-lsp/tests/live_engine_gate.rs"]
mod live_engine_gate;

#[tokio::test]
async fn test_edge_rust_struct_usages_and_call_directions_agree_on_the_same_engine() -> TestResult {
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let files = [
        (
            "Cargo.toml",
            "[package]\nname='edge_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n",
        ),
        (
            "lib.rs",
            "pub trait Task { fn area(&self); }\npub struct Worker;\nimpl Task for Worker { fn area(&self) {} }\nimpl Worker { pub fn run(&self) { self.area(); } }\npub fn take(worker: Worker) { worker.run(); }\n",
        ),
    ];
    // This fixture reads local declarations without cache priming or check diagnostics.
    let configuration = "[languages.rust.lsp]\ncommand=['rustup','run','1.98','rust-analyzer']\ninitialization_options={cachePriming={enable=false},checkOnSave=false}\n";
    let (_directory, client, task) =
        served_workspace(&files, Some(configuration.to_owned())).await?;
    let worker = captured_seed(&client, "Worker", "lib.rs").await?;
    let run = captured_seed(&client, "run", "lib.rs").await?;
    let usages = QueryParams {
        query: format!("SELECT ?usage ?kind {{ <{worker}> :usages ?usage . ?usage :kind ?kind }}"),
        language: QueryLanguage::Sparql,
        page: 1,
        max_results: 10,
        revision: None,
    };
    let usages = query_after_population(&client, &usages).await?;
    assert!(
        !workspace_client::query::rows(&usages)?.is_empty(),
        "{usages}"
    );
    let outgoing = QueryParams {
        query: format!("SELECT ?target {{ <{run}> :calls ?target }}"),
        language: QueryLanguage::Sparql,
        page: 1,
        max_results: 10,
        revision: None,
    };
    let outgoing = query_after_population(&client, &outgoing).await?;
    let targets = workspace_client::query::rows(&outgoing)?;
    assert!(!targets.is_empty(), "{outgoing}");
    for row in targets {
        let target = workspace_client::query::iri(row, "target")?;
        let incoming = QueryParams {
            query: format!("SELECT ?caller {{ ?caller :calls <{target}> }}"),
            language: QueryLanguage::Sparql,
            page: 1,
            max_results: 10,
            revision: None,
        };
        let incoming = query_after_population(&client, &incoming).await?;
        assert!(
            workspace_client::query::rows(&incoming)?
                .iter()
                .any(|row| workspace_client::query::iri(row, "caller")
                    .is_ok_and(|caller| caller == run)),
            "{incoming}"
        );
    }
    let trait_area = worker.replace("/Worker", "/Task/area");
    let incoming = QueryParams {
        query: format!("SELECT ?caller {{ ?caller :calls <{trait_area}> }}"),
        language: QueryLanguage::Sparql,
        page: 1,
        max_results: 10,
        revision: None,
    };
    let incoming = query_after_population(&client, &incoming).await?;
    let caller = workspace_client::query::rows(&incoming)?
        .iter()
        .any(|row| workspace_client::query::iri(row, "caller").is_ok_and(|caller| caller == run));
    let target = targets.iter().any(|row| {
        workspace_client::query::iri(row, "target").is_ok_and(|target| target == trait_area)
    });
    assert_eq!(caller, target, "incoming {incoming}; outgoing {outgoing}");
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[cfg(unix)]
const EDGE_READY_ENGINE: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
range() { printf '{"start":{"line":%s,"character":%s},"end":{"line":%s,"character":%s}}' "$1" "$2" "$1" "$3"; }
item() { printf '{"name":"%s","kind":%s,"uri":"%s","range":%s,"selectionRange":%s}' "$1" "$2" "$uri" "$(range "$3" 0 "$5")" "$(range "$3" "$4" "$5")"; }
cold=1
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"definitionProvider\":true,\"callHierarchyProvider\":true,\"implementationProvider\":true,\"typeHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"edge/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"method":"textDocument/didOpen"'*)
      uri=$(printf '%s' "$body" | grep -o '"uri":"[^"]*"' | head -1 | cut -d '"' -f4) ;;
    *'"method":"textDocument/prepareCallHierarchy"'*)
      if [ "$cold" = 1 ]; then
        frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}"
        frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"edge/analysis","value":{"kind":"end"}}}'
        cold=0
      else
        if printf '%s' "$body" | grep -q '"line":1'; then prepared=$(item leaf 12 1 7 11); else prepared=$(item root 12 2 7 11); fi
        frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[$prepared]}"
      fi ;;
    *'"method":"callHierarchy/outgoingCalls"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":$(item leaf 12 1 7 11),\"fromRanges\":[$(range 2 30 38)]}]}" ;;
    *'"method":"callHierarchy/incomingCalls"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"from\":$(item root 12 2 7 11),\"fromRanges\":[$(range 2 30 38)]}]}" ;;
    *'"method":"textDocument/definition"'*)
      if printf '%s' "$body" | grep -q '"line":3'; then resolved=$(range 0 11 17); else resolved=$(range 1 7 11); fi
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$uri\",\"range\":$resolved}]}" ;;
    *'"method":"textDocument/references"'*)
      if printf '%s' "$body" | grep -q '"line":0'; then result="[{\"uri\":\"$uri\",\"range\":$(range 3 20 26)}]"; else result='[]'; fi
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":$result}" ;;
    *'"method":"textDocument/implementation"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$uri\",\"range\":$(range 0 11 17)}]}" ;;
    *'"method":"textDocument/prepareTypeHierarchy"'*)
      if printf '%s' "$body" | grep -q '"line":4'; then prepared=$(item Task 11 4 10 14); else prepared=$(item Worker 23 0 11 17); fi
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[$prepared]}" ;;
    *'"method":"typeHierarchy/supertypes"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[$(item Base 5 5 11 15)]}" ;;
    *'"method":"typeHierarchy/subtypes"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[$(item Worker 23 0 11 17)]}" ;;
    *'"id":'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#;

#[cfg(unix)]
#[tokio::test]
async fn test_edge_cold_calls_wait_and_reference_answers_need_no_declaration_marker() -> TestResult
{
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("edges.sh");
    std::fs::write(&script, EDGE_READY_ENGINE)?;
    let command = serde_json::to_string(&script)?;
    let configuration = format!("[languages.rust.lsp]\ncommand=['sh',{command}]\n");
    let files = [
        (
            "Cargo.toml",
            "[package]\nname='edge_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n",
        ),
        (
            "lib.rs",
            "pub struct Worker;\npub fn leaf() {}\npub fn root(callback: fn()) { callback(); }\npub fn take(worker: Worker) {}\npub trait Task {}\npub struct Base;\nimpl Task for Worker {}\n",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, Some(configuration)).await?;
    let root = captured_seed(&client, "root", "lib.rs").await?;
    let leaf = captured_seed(&client, "leaf", "lib.rs").await?;
    let worker = captured_seed(&client, "Worker", "lib.rs").await?;
    let take = captured_seed(&client, "take", "lib.rs").await?;
    let base = captured_seed(&client, "Base", "lib.rs").await?;
    let trait_edge = captured_seed(&client, "Task", "lib.rs").await?;
    for (query, target) in [
        (
            format!("SELECT ?target {{ <{root}> :calls ?target }}"),
            Some(leaf.clone()),
        ),
        (
            format!("SELECT ?target {{ ?target :calls <{leaf}> }}"),
            Some(root),
        ),
        (
            format!("SELECT ?usage {{ <{worker}> :usages ?usage }}"),
            None,
        ),
        (
            format!("SELECT ?target {{ <{take}> :references ?target }}"),
            Some(worker.clone()),
        ),
        (
            format!("SELECT ?target {{ <{worker}> :extends ?target }}"),
            Some(base),
        ),
        (
            format!("SELECT ?target {{ ?target :implements <{trait_edge}> }}"),
            Some(worker),
        ),
    ] {
        let answer = query_after_population(
            &client,
            &QueryParams {
                query,
                language: QueryLanguage::Sparql,
                page: 1,
                max_results: 10,
                revision: None,
            },
        )
        .await?;
        let rows = workspace_client::query::rows(&answer)?;
        assert!(!rows.is_empty(), "{answer}");
        if let Some(target) = target {
            assert!(
                rows.iter()
                    .any(|row| workspace_client::query::iri(row, "target")
                        .is_ok_and(|found| found == target)),
                "{answer}"
            );
        }
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_edge_queries_page_targets_at_the_same_revision() -> TestResult {
    let source =
        "export function root() { alpha(); beta(); }\nfunction alpha() {}\nfunction beta() {}\n";
    let (_directory, client, task) = served_workspace(&[("entry.mjs", source)], None).await?;
    let root = captured_seed(&client, "root", "entry.mjs").await?;
    let mut params = QueryParams {
        query: format!("SELECT ?target {{ <{root}> :calls ?target }} ORDER BY ?target"),
        language: QueryLanguage::Sparql,
        page: 1,
        max_results: 1,
        revision: None,
    };
    let first = query_after_population(&client, &params).await?;
    assert_eq!(
        first["pagination"],
        json!({"page": 1, "pages": 2, "max_results": 1, "total": 2}),
        "{first}"
    );
    let revision = first["revision"].clone();
    params.revision = Some(serde_json::from_value(revision.clone())?);
    params.page = 2;
    let second = query_after_population(&client, &params).await?;
    assert_eq!(second["revision"], revision);
    assert_eq!(
        second["pagination"],
        json!({"page": 2, "pages": 2, "max_results": 1, "total": 2}),
        "{second}"
    );
    let first_target =
        workspace_client::query::iri(&workspace_client::query::rows(&first)?[0], "target")?;
    let second_target =
        workspace_client::query::iri(&workspace_client::query::rows(&second)?[0], "target")?;
    assert!(first_target < second_target, "{first}; {second}");
    params.page = 3;
    let empty = query_after_population(&client, &params).await?;
    assert_eq!(empty["revision"], revision);
    assert!(workspace_client::query::rows(&empty)?.is_empty(), "{empty}");
    assert_eq!(
        empty["pagination"],
        json!({"page": 3, "pages": 2, "max_results": 1, "total": 2}),
        "{empty}"
    );
    client.cancel().await?;
    task.await?;
    Ok(())
}
