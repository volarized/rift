//! Edge queries through the public MCP client.

#[expect(
    dead_code,
    reason = "shared workspace helpers serve several integration suites"
)]
mod workspace_client;

mod hermetic_search;

use rift_protocol::query::{QueryLanguage, QueryParams};
use serde_json::json;
use std::fmt::Write as _;
use workspace_client::{TestResult, captured_seed, query_after_population, served_workspace};

#[tokio::test]
async fn test_typescript_deferred_calls_and_declared_receivers_are_queryable() -> TestResult {
    let (_directory, client, task) = served_workspace(
        &[
            ("entry.ts", "import type { Box } from './types.js'; let value = 0; export function root(input: Box) { value += 1; input.run(); later(); return input.value; } const later = () => {};"),
            ("types.ts", "export interface Box { run(): void; value: number; }"),
        ],
        None,
    ).await?;
    let params = QueryParams {
        query: "SELECT ?name { ?source :calls ?target . ?target :name ?name } ORDER BY ?name"
            .into(),
        ..QueryParams::default()
    };
    let answer = query_after_population(&client, &params).await?;
    let rows = workspace_client::query::rows(&answer)?;
    let names = rows
        .iter()
        .map(|row| workspace_client::query::literal(row, "name"))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(names, ["later", "run"], "{answer}");
    let params = QueryParams {
        query: "SELECT (COUNT(*) AS ?n) { ?source :writes ?target }".into(),
        ..QueryParams::default()
    };
    let answer = query_after_population(&client, &params).await?;
    let rows = workspace_client::query::rows(&answer)?;
    assert_eq!(workspace_client::query::literal(&rows[0], "n")?, "1");
    let params = QueryParams {
        query: "SELECT (COUNT(DISTINCT ?target) AS ?n) { ?source :calls ?target . ?edge a :Edge; :from ?source; :to ?target; :kinds :calls; :provider 'static' }".into(),
        ..QueryParams::default()
    };
    let answer = query_after_population(&client, &params).await?;
    let rows = workspace_client::query::rows(&answer)?;
    assert_eq!(
        workspace_client::query::literal(&rows[0], "n")?,
        "2",
        "{answer}"
    );
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_static_usage_records_keep_kind_and_enclosing_symbol_together() -> TestResult {
    let (_directory, client, task) = served_workspace(
        &[
            ("entry.ts", "export * as tools from './tools.js';"),
            ("tools.ts", "export const value = 1;"),
        ],
        None,
    )
    .await?;
    let params = QueryParams {
        query: "SELECT ?source ?kind { ?target :usages ?usage . ?usage :of ?source; :kind ?kind } ORDER BY ?source ?kind".into(),
        ..QueryParams::default()
    };
    let answer = query_after_population(&client, &params).await?;
    let rows = workspace_client::query::rows(&answer)?;
    assert_eq!(rows.len(), 2, "{answer}");
    for (row, (source, kind)) in rows.iter().zip([
        ("rift://symbol/local/typescript/.%2Fentry.ts", "imports"),
        (
            "rift://symbol/local/typescript/.%2Fentry.ts/tools",
            "aliases",
        ),
    ]) {
        assert_eq!(workspace_client::query::iri(row, "source")?, source);
        assert_eq!(workspace_client::query::literal(row, "kind")?, kind);
    }
    let params = QueryParams {
        query: "SELECT (COUNT(?usage) AS ?n) { ?target :usages ?usage }".into(),
        ..QueryParams::default()
    };
    let answer = query_after_population(&client, &params).await?;
    let rows = workspace_client::query::rows(&answer)?;
    assert_eq!(workspace_client::query::literal(&rows[0], "n")?, "2");
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_static_inventory_projects_repeated_occurrences_with_dataset_bounds() -> TestResult {
    let count = 10_001;
    let source = format!(
        "function leaf() {{}}\nexport function root() {{ {} }}\n",
        "leaf();".repeat(count)
    );
    let (_directory, client, task) = served_workspace(&[("entry.ts", &source)], None).await?;
    // Each invocation contributes a call occurrence and a callee read.
    for (predicate, expected) in [("calls", 1), ("references", 1), ("usages", 2 * count)] {
        let params = QueryParams {
            query: format!("SELECT (COUNT(*) AS ?count) {{ ?s :{predicate} ?t }}"),
            ..QueryParams::default()
        };
        let answer = query_after_population(&client, &params).await?;
        let rows = workspace_client::query::rows(&answer)?;
        assert_eq!(rows.len(), 1, "{params:?}: {answer}");
        assert_eq!(
            workspace_client::query::literal(&rows[0], "count")?,
            expected.to_string(),
            "{params:?}: {answer}"
        );
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_typescript_static_facts_are_read_through_sparql() -> TestResult {
    let files = [
        (
            "entry.ts",
            "import * as core from './index.js';\ninterface Value {}\ninterface Task extends Value {}\nclass Derived extends core.Base<Value> { run(input: Value): Value { this.value = core.value; this.leaf(); return this.value; } }\nexport function root() { core.leaf(); new core.Base(); }",
        ),
        ("index.ts", "export * from './base.js';"),
        (
            "base.ts",
            "export const value = 1;\nexport function leaf() {}\nexport class Base<T> { value: T; leaf() {} run(input: T): T { return input; } }",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    let derived = captured_seed(&client, "Derived", "entry.ts").await?;
    let root = captured_seed(&client, "root", "entry.ts").await?;
    let run = captured_seed(&client, "run", "entry.ts").await?;
    for query in [
        format!("ASK {{ <{derived}> :extends ?base . ?base :name 'Base' }}"),
        format!("ASK {{ <{derived}> :contains ?member . ?member :name 'run' }}"),
        format!("ASK {{ <{root}> :calls ?target . ?target :name 'leaf' }}"),
        format!("ASK {{ <{run}> :overrides ?target }}"),
        format!("ASK {{ <{run}> :reads ?target . ?target :name 'value' }}"),
        format!("ASK {{ <{run}> :writes ?target . ?target :name 'value' }}"),
        format!("ASK {{ <{run}> :hasType ?target . ?target :name 'Value' }}"),
        "ASK { ?symbol :imports ?target }".into(),
        "ASK { ?alias :name 'leaf'; :aliases ?target . ?target :name 'leaf' }".into(),
    ] {
        let params = QueryParams {
            query,
            language: QueryLanguage::Sparql,
            page: 1,
            max_results: 10,
            revision: None,
            include: rift_protocol::query::QueryInclude::default(),
        };
        let answer = query_after_population(&client, &params).await?;
        assert_eq!(
            answer["answer"],
            json!({"result":"boolean", "value":true}),
            "{params:?}: {answer}"
        );
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}

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
            include: rift_protocol::query::QueryInclude::default(),
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
            include: rift_protocol::query::QueryInclude::default(),
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
  length=${header%?}; length=${length##*[!0-9]}
  body=$(dd bs=1 count="$length" 2>/dev/null)
  case "$body" in *'"id":'*) id=${body#*\"id\":}; id=${id%%[!0-9]*} ;; *) id= ;; esac
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
            include: rift_protocol::query::QueryInclude::default(),
            query: text,
            language: QueryLanguage::Sparql,
            page: 1,
            max_results: 10,
            revision: None,
        };
        let (answer, warnings) =
            workspace_client::populated_answer(&client, "search", &serde_json::to_value(&params)?)
                .await?;
        assert!(answer.get("warnings").is_none());
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

/// A `sh` engine serving call hierarchy that holds each call hierarchy answer for 300 ms
/// during its first 2 s and answers at once afterwards: every item it prepares calls nothing.
/// It announces and ends its work at start, and appends `start` to the file its first
/// argument names at each start.
#[cfg(unix)]
const SLOW_BATCH_ENGINE: &str = r#"echo start >> "$1"
started=$(date +%s)
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=${header%?}; length=${length##*[!0-9]}
  body=$(dd bs=1 count="$length" 2>/dev/null)
  case "$body" in *'"id":'*) id=${body#*\"id\":}; id=${id%%[!0-9]*} ;; *) id= ;; esac
  case "$body" in *'"uri":"'*) uri=${body#*\"uri\":\"}; uri=${uri%%\"*} ;; *) uri= ;; esac
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"callHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}'
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}' ;;
    *'"method":"$/cancelRequest"'*) ;;
    *'"method":"textDocument/prepareCallHierarchy"'*)
      if [ $(( $(date +%s) - started )) -lt 2 ]; then sleep 0.3; fi
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"entry\",\"kind\":12,\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":16}},\"selectionRange\":{\"start\":{\"line\":0,\"character\":7},\"end\":{\"line\":0,\"character\":12}}}]}" ;;
    *'"method":"callHierarchy/outgoingCalls"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'[0-9]*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#;

/// Inventory reads cut at a 1 s `[server] readiness_timeout` by an engine that answers each
/// batch late keep the engine: four reads in a row meet one engine process, and once the
/// engine answers at once a later read carries no `engine_analysis_unavailable`.
#[cfg(unix)]
#[tokio::test]
async fn test_edge_inventory_reads_cut_at_their_deadline_keep_the_engine() -> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let log = engine.path().join("engine.log");
    std::fs::write(&script, SLOW_BATCH_ENGINE)?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n",
        script.display(),
        log.display()
    );
    let files = [
        (
            "Cargo.toml",
            "[package]\nname='edge_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n",
        ),
        (
            "lib.rs",
            "pub fn entry() { one::entry(); }\npub mod one;\npub mod two;\n",
        ),
        ("one.rs", "pub fn entry() { crate::two::entry(); }\n"),
        ("two.rs", "pub fn entry() {}\n"),
    ];
    let (_directory, client, task) = served_workspace(&files, Some(configuration)).await?;
    let params = QueryParams {
        query: "SELECT ?s ?t { ?s :calls ?t }".to_owned(),
        language: QueryLanguage::Sparql,
        page: 1,
        max_results: 100,
        revision: None,
        include: rift_protocol::query::QueryInclude::default(),
    };
    for _read in 0..4 {
        query_after_population(&client, &params).await?;
    }
    let mut settled = None;
    for _read in 0..20 {
        let (answer, warnings) =
            workspace_client::populated_answer(&client, "search", &serde_json::to_value(&params)?)
                .await?;
        assert!(answer.get("warnings").is_none());
        let unavailable = warnings
            .iter()
            .any(|warning| warning["code"] == "engine_analysis_unavailable");
        if !unavailable {
            settled = Some(answer);
            break;
        }
    }
    settled.ok_or("a read after the slow answers takes the whole engine inventory")?;
    client.cancel().await?;
    task.await?;
    let recorded = std::fs::read_to_string(&log)?;
    assert_eq!(
        recorded.lines().filter(|line| *line == "start").count(),
        1,
        "every read meets the one engine process: {recorded}"
    );
    Ok(())
}

/// The `sh` engine of [`SLOW_BATCH_ENGINE`], which also reads no document of 64 KiB or more
/// for 3 s, during each of its first four starts: a read that opens such a document is
/// dropped at its deadline with the frame part-written, and the slot discards the session.
#[cfg(unix)]
const SLOW_READER_ENGINE: &str = r#"echo start >> "$1"
starts=$(grep -c start "$1")
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=${header%?}; length=${length##*[!0-9]}
  if [ "$length" -ge 65536 ] && [ "$starts" -le 4 ]; then sleep 3; fi
  body=$(dd bs=1 count="$length" 2>/dev/null)
  case "$body" in *'"id":'*) id=${body#*\"id\":}; id=${id%%[!0-9]*} ;; *) id= ;; esac
  case "$body" in *'"uri":"'*) uri=${body#*\"uri\":\"}; uri=${uri%%\"*} ;; *) uri= ;; esac
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"callHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}'
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}' ;;
    *'"method":"$/cancelRequest"'*) ;;
    *'"method":"textDocument/prepareCallHierarchy"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"entry\",\"kind\":12,\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":16}},\"selectionRange\":{\"start\":{\"line\":0,\"character\":7},\"end\":{\"line\":0,\"character\":12}}}]}" ;;
    *'"method":"callHierarchy/outgoingCalls"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'[0-9]*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#;

/// Inventory reads dropped at their deadline while a document's frame is part-written lose
/// their session, and the slot replaces it without charging the restart budget: after four
/// such reads a later read still meets an engine and carries no
/// `engine_analysis_unavailable`.
#[cfg(unix)]
#[tokio::test]
async fn test_edge_inventory_reads_dropped_mid_write_do_not_spend_the_restart_budget() -> TestResult
{
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let log = engine.path().join("engine.log");
    std::fs::write(&script, SLOW_READER_ENGINE)?;
    // A healthy replacement settles well inside the same deadline that cuts the stalled write.
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [languages.rust.lsp]\nsettle_delay = \"10ms\"\ncommand = [\"sh\", \"{}\", \"{}\"]\n",
        script.display(),
        log.display()
    );
    let large = format!("pub fn entry() {{}}\n{}", "// padding\n".repeat(6_200));
    let files = [
        (
            "Cargo.toml",
            "[package]\nname='edge_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n",
        ),
        ("lib.rs", large.as_str()),
    ];
    let (_directory, client, task) = served_workspace(&files, Some(configuration)).await?;
    let params = QueryParams {
        query: "SELECT ?s ?t { ?s :calls ?t }".to_owned(),
        language: QueryLanguage::Sparql,
        page: 1,
        max_results: 100,
        revision: None,
        include: rift_protocol::query::QueryInclude::default(),
    };
    for _read in 0..4 {
        query_after_population(&client, &params).await?;
    }
    let mut settled = None;
    for _read in 0..10 {
        let (answer, warnings) =
            workspace_client::populated_answer(&client, "search", &serde_json::to_value(&params)?)
                .await?;
        assert!(answer.get("warnings").is_none());
        let unavailable = warnings
            .iter()
            .any(|warning| warning["code"] == "engine_analysis_unavailable");
        if !unavailable {
            settled = Some(answer);
            break;
        }
    }
    let recorded = std::fs::read_to_string(&log)?;
    settled.ok_or_else(|| {
        format!("a read after the dropped ones meets a running engine; starts: {recorded}")
    })?;
    client.cancel().await?;
    task.await?;
    Ok(())
}

/// A `sh` engine serving references and definitions over `lib.rs` and `user.rs`, which answers
/// a definition only for a document it was told is open, as typescript-language-server does.
/// The references of `beacon` name its declaration and its call in `user.rs`; a definition
/// asked inside an open document names `beacon` in `lib.rs`. It appends `open <path>` and
/// `close <path>` to the file its first argument names for each document notification.
#[cfg(unix)]
const OPEN_DOCUMENTS_ENGINE: &str = r#"opened=
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=${header%?}; length=${length##*[!0-9]}
  body=$(dd bs=1 count="$length" 2>/dev/null)
  case "$body" in *'"id":'*) id=${body#*\"id\":}; id=${id%%[!0-9]*} ;; *) id= ;; esac
  case "$body" in *'"uri":"'*) uri=${body#*\"uri\":\"}; uri=${uri%%\"*} ;; *) uri= ;; esac
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"definitionProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}'
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}' ;;
    *'"method":"textDocument/didOpen"'*)
      opened="$opened $uri"
      echo "open ${uri##*/}" >> "$1" ;;
    *'"method":"textDocument/didClose"'*)
      opened=$(printf '%s' "$opened" | sed "s| $uri||")
      echo "close ${uri##*/}" >> "$1" ;;
    *'"method":"$/cancelRequest"'*) ;;
    *'"method":"textDocument/references"'*)
      base=${uri%/*}
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$base/lib.rs\",\"range\":{\"start\":{\"line\":0,\"character\":7},\"end\":{\"line\":0,\"character\":13}}},{\"uri\":\"$base/user.rs\",\"range\":{\"start\":{\"line\":0,\"character\":23},\"end\":{\"line\":0,\"character\":29}}}]}" ;;
    *'"method":"textDocument/definition"'*)
      base=${uri%/*}
      case "$opened " in
        *" $uri "*)
          frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$base/lib.rs\",\"range\":{\"start\":{\"line\":0,\"character\":7},\"end\":{\"line\":0,\"character\":13}}}]}" ;;
        *)
          frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
      esac ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'[0-9]*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#;

/// A reference the engine names in a document the read did not open is resolved once the
/// read opens that document with the index's bytes: the fact names the engine among its
/// providers, no reference location is dropped, and the engine reads the document's
/// `didOpen` before its `didClose`.
#[cfg(unix)]
#[tokio::test]
async fn test_edge_reference_locations_in_unopened_documents_are_resolved() -> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let log = engine.path().join("engine.log");
    std::fs::write(&script, OPEN_DOCUMENTS_ENGINE)?;
    let configuration = format!(
        "[languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n",
        script.display(),
        log.display()
    );
    let files = [
        (
            "Cargo.toml",
            "[package]\nname='edge_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n",
        ),
        ("lib.rs", "pub fn beacon() {}\npub mod user;\n"),
        ("user.rs", "pub fn user() { crate::beacon(); }\n"),
    ];
    let (_directory, client, task) = served_workspace(&files, Some(configuration)).await?;
    let beacon = captured_seed(&client, "beacon", "lib.rs").await?;
    let user = captured_seed(&client, "user", "user.rs").await?;
    let params = QueryParams {
        query: format!(
            "SELECT ?s ?p {{ ?s :references <{beacon}> . ?e a :Edge; :from ?s; :to <{beacon}>; \
                 :kinds :references; :provider ?p }}"
        ),
        language: QueryLanguage::Sparql,
        page: 1,
        max_results: 100,
        revision: None,
        include: rift_protocol::query::QueryInclude::default(),
    };
    let (answer, warnings) =
        workspace_client::populated_answer(&client, "search", &serde_json::to_value(&params)?)
            .await?;
    assert!(answer.get("warnings").is_none());
    assert!(
        !warnings
            .iter()
            .any(|warning| warning["code"] == "engine_analysis_unavailable"),
        "no reference location is dropped: {answer}"
    );
    let providers: Vec<&str> = workspace_client::query::rows(&answer)?
        .iter()
        .filter(|row| workspace_client::query::iri(row, "s").is_ok_and(|from| from == user))
        .filter_map(|row| workspace_client::query::literal(row, "p").ok())
        .collect();
    assert!(
        providers.contains(&"lsp"),
        "the reference in user.rs names the engine: {answer}"
    );
    client.cancel().await?;
    task.await?;
    let recorded = std::fs::read_to_string(&log)?;
    let lines: Vec<&str> = recorded.lines().collect();
    let opened = lines.iter().position(|line| *line == "open user.rs");
    let closed = lines.iter().position(|line| *line == "close user.rs");
    assert!(
        opened.is_some_and(|opened| closed.is_some_and(|closed| opened < closed)),
        "the read opens user.rs before it asks there and closes it after: {lines:?}"
    );
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
        include: rift_protocol::query::QueryInclude::default(),
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
        include: rift_protocol::query::QueryInclude::default(),
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
            include: rift_protocol::query::QueryInclude::default(),
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
        include: rift_protocol::query::QueryInclude::default(),
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
  length=${header%?}; length=${length##*[!0-9]}
  body=$(dd bs=1 count="$length" 2>/dev/null)
  case "$body" in *'"id":'*) id=${body#*\"id\":}; id=${id%%[!0-9]*} ;; *) id= ;; esac
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"definitionProvider\":true,\"callHierarchyProvider\":true,\"implementationProvider\":true,\"typeHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"edge/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"method":"textDocument/didOpen"'*)
      case "$body" in *'"uri":"'*) uri=${body#*\"uri\":\"}; uri=${uri%%\"*} ;; *) uri= ;; esac ;;
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
            Some(worker.clone()),
        ),
        (
            format!(
                "SELECT ?target {{ ?target :implements ?trait FILTER(?trait = <{trait_edge}>) }}"
            ),
            Some(worker),
        ),
    ] {
        let answer = query_after_population(
            &client,
            &QueryParams {
                include: rift_protocol::query::QueryInclude::default(),
                query: query.clone(),
                language: QueryLanguage::Sparql,
                page: 1,
                max_results: 10,
                revision: None,
            },
        )
        .await?;
        let rows = workspace_client::query::rows(&answer)?;
        assert!(!rows.is_empty(), "{query}: {answer}");
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
        include: rift_protocol::query::QueryInclude::default(),
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

#[tokio::test]
async fn test_python_reexports_keep_imports_and_aliases() -> TestResult {
    let files = [
        ("pkg/__init__.py", "from .models import Value as Value\n"),
        ("pkg/models.py", "class Value: pass\n"),
        (
            "main.py",
            "from pkg import Value\ndef build(): return Value()\n",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    for query in [
        "ASK { <rift://symbol/local/python/pkg> :imports <rift://symbol/local/python/pkg/models/Value> }",
        "ASK { <rift://symbol/local/python/pkg/Value> :aliases <rift://symbol/local/python/pkg/models/Value> }",
        "ASK { <rift://symbol/local/python/main/build> :calls <rift://symbol/local/python/pkg/models/Value> }",
    ] {
        let params = QueryParams {
            query: query.to_owned(),
            language: QueryLanguage::Sparql,
            ..Default::default()
        };
        let answer = query_after_population(&client, &params).await?;
        assert_eq!(
            answer["answer"],
            json!({"result":"boolean", "value":true}),
            "{query}: {answer}"
        );
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_python_rebindings_share_the_existing_symbol_facets() -> TestResult {
    let source = "shared = 0\ndef root():\n    active = False\n    def finish():\n        nonlocal active\n        active = True\n    def publish():\n        global shared\n        shared = 1\n";
    let (_directory, client, task) = served_workspace(&[("main.py", source)], None).await?;
    for expected in [
        "rift://symbol/local/python/main/root/active",
        "rift://symbol/local/python/main/shared",
    ] {
        let query = QueryParams {
            query: format!("ASK {{ <{expected}> a :Symbol }}"),
            language: QueryLanguage::Sparql,
            ..Default::default()
        };
        let answer = query_after_population(&client, &query).await?;
        assert_eq!(
            answer["answer"],
            json!({"result":"boolean", "value":true}),
            "{answer}"
        );
        let params = rift_protocol::read::FacetsParams::of(rift_protocol::read::SymbolId(
            expected.to_owned(),
        ));
        let request = workspace_client::tool_request("facets", &serde_json::to_value(params)?);
        let facets = workspace_client::call_retrying_acceptance(&client, request).await?;
        assert_eq!(facets["pagination"]["total"], json!(2), "{facets}");
    }
    let params = QueryParams {
        query: "ASK { <rift://symbol/local/python/main/root/finish/active> a :Symbol }".to_owned(),
        language: QueryLanguage::Sparql,
        ..Default::default()
    };
    let answer = query_after_population(&client, &params).await?;
    assert_eq!(
        answer["answer"],
        json!({"result":"boolean", "value":false}),
        "{answer}"
    );
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_python_static_member_edges_are_read_through_sparql() -> TestResult {
    let source = "import models\nclass Base:\n    value: int = 0\n    def run(self): pass\nclass Child(Base):\n    def run(self, value: models.Value) -> models.Value:\n        self.value = value\n        self.value += 1\n        return models.current\ndef typed(value: models.Value):\n    value.run()\n    value.item = 1\n    models.current = value\n    return value.item\ndef build() -> models.Value | None: return None\ndef alias():\n    module = models\n    value = build()\n    second = value\n    second.item = 2\n    module.current = second\n    return second.item\ndef checked(value):\n    if not isinstance(value, models.Value): return\n    value.item = 3\n";
    let files = [
        ("main.py", source),
        (
            "models.py",
            "class Value:\n    item: int = 0\n    def run(self): pass\ncurrent: Value\n",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    captured_seed(&client, "Child", "main.py").await?;
    for query in [
        "ASK { <rift://symbol/local/python/main/alias> :writes <rift://symbol/local/python/models/Value/item> }",
        "ASK { <rift://symbol/local/python/main/alias> :reads <rift://symbol/local/python/models/Value/item> }",
        "ASK { <rift://symbol/local/python/main/alias> :writes <rift://symbol/local/python/models/current> }",
        "ASK { <rift://symbol/local/python/main/checked> :writes <rift://symbol/local/python/models/Value/item> }",
        "ASK { <rift://symbol/local/python/main/typed> :calls <rift://symbol/local/python/models/Value/run> }",
        "ASK { <rift://symbol/local/python/main/typed> :reads <rift://symbol/local/python/models/Value/item> }",
        "ASK { <rift://symbol/local/python/main/typed> :writes <rift://symbol/local/python/models/Value/item> }",
        "ASK { <rift://symbol/local/python/main/typed> :writes <rift://symbol/local/python/models/current> }",
        "ASK { <rift://symbol/local/python/main> :contains <rift://symbol/local/python/main/Child> }",
        "ASK { <rift://symbol/local/python/main/Child> :contains <rift://symbol/local/python/main/Child/run> }",
        "ASK { <rift://symbol/local/python/main/Child/run> :overrides <rift://symbol/local/python/main/Base/run> }",
        "ASK { <rift://symbol/local/python/main/Child/run> :reads <rift://symbol/local/python/models/current> }",
        "ASK { <rift://symbol/local/python/main/Child/run> :writes <rift://symbol/local/python/main/Base/value> }",
        "ASK { <rift://symbol/local/python/main/Child/run> :hasType <rift://symbol/local/python/models/Value> }",
        "ASK { <rift://symbol/local/python/main> :imports <rift://symbol/local/python/models> }",
    ] {
        let params = QueryParams {
            query: query.to_owned(),
            language: QueryLanguage::Sparql,
            ..Default::default()
        };
        let answer = query_after_population(&client, &params).await?;
        assert_eq!(
            answer["answer"],
            json!({"result":"boolean", "value":true}),
            "{query}: {answer}"
        );
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_python_static_inventory_survives_dependency_context_and_many_symbols() -> TestResult {
    let mut source = "def leaf(): pass\n".to_owned();
    for index in 0..70 {
        writeln!(source, "def root{index}(): leaf()")?;
    }
    for context in [None, Some("[[dependencies.packages]]\nmanager = 'pypi'\nregistry = 'pypi.org'\nname = 'httpx'\nversion = '0.28.1'\n".to_owned())] {
        let (_directory, client, task) = served_workspace(&[("main.py", &source)], context).await?;
        captured_seed(&client, "leaf", "main.py").await?;
        for predicate in ["calls", "references"] {
            let params = QueryParams { query: format!("SELECT ?from ?to {{ ?from :{predicate} ?to }}"), language: QueryLanguage::Sparql, ..Default::default() };
            let answer = query_after_population(&client, &params).await?;
            assert_eq!(answer["pagination"]["total"], json!(70), "{predicate}: {answer}");
        }
        client.cancel().await?;
        task.await?;
    }
    Ok(())
}

#[tokio::test]
async fn test_python_declarations_include_locals_empty_modules_and_project_files() -> TestResult {
    let files = [
        ("pkg/empty.py", ""),
        (
            "docs_src/tutorial.py",
            "class Item: pass\ndef build():\n    visited: list[Item] = []\n",
        ),
        (
            "pyproject.toml",
            "[build-system]\nrequires=['pdm-backend']\nbuild-backend='pdm.backend'\n",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    for (name, path, identity) in [
        (
            "Item",
            "docs_src/tutorial.py",
            "rift://symbol/local/python/docs_src/tutorial/Item",
        ),
        (
            "visited",
            "docs_src/tutorial.py",
            "rift://symbol/local/python/docs_src/tutorial/build/visited",
        ),
    ] {
        assert_eq!(captured_seed(&client, name, path).await?, identity);
    }
    let params = QueryParams {
        query: "ASK { <rift://symbol/local/python/pkg/empty> a :Symbol }".to_owned(),
        language: QueryLanguage::Sparql,
        page: 1,
        max_results: 10,
        revision: None,
        include: rift_protocol::query::QueryInclude::default(),
    };
    let answer = query_after_population(&client, &params).await?;
    assert_eq!(answer["answer"], json!({"result":"boolean", "value":true}));
    let symbol = rift_protocol::read::SymbolId("rift://symbol/local/python/pkg/empty".to_owned());
    let params = rift_protocol::read::FacetsParams::of(symbol);
    let request = workspace_client::tool_request("facets", &serde_json::to_value(params)?);
    let facets = workspace_client::call_retrying_acceptance(&client, request).await?;
    assert_eq!(facets["pagination"]["total"], json!(1), "{facets}");
    assert_eq!(facets["declarations"][0]["source"]["text"], json!(""));
    assert_eq!(
        facets["declarations"][0]["range"],
        json!({"start":0,"end":0})
    );
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_python_local_initializer_calls_start_at_the_enclosing_callable() -> TestResult {
    let source = "def leaf(): return 1\ndef root():\n    value: int = leaf()\n    return value\n";
    let (_directory, client, task) = served_workspace(&[("main.py", source)], None).await?;
    let root = captured_seed(&client, "root", "main.py").await?;
    let leaf = captured_seed(&client, "leaf", "main.py").await?;
    let params = QueryParams {
        query: format!("ASK {{ <{root}> :calls <{leaf}> }}"),
        language: QueryLanguage::Sparql,
        page: 1,
        max_results: 10,
        revision: None,
        include: rift_protocol::query::QueryInclude::default(),
    };
    let answer = query_after_population(&client, &params).await?;
    assert_eq!(answer["answer"], json!({"result":"boolean", "value":true}));
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn test_edge_rust_static_inventory_and_multiple_facets_are_queryable() -> TestResult {
    let mut source = String::from(
        "mod jobs; use crate::jobs::{Task, leaf}; trait Run { fn execute(&self); } \
         pub fn root(task: &Task) { leaf(); Task::new(); task.execute(); }\n",
    );
    for index in 0..70 {
        writeln!(source, "pub fn extra_{index}() {{ leaf(); }}")?;
    }
    let files = [
        ("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n"),
        ("src/lib.rs", source.as_str()),
        (
            "src/jobs.rs",
            "use crate::Run; pub struct Value; impl Value { fn open(&self) {} } \
             pub struct Task { value: Value } pub fn leaf() {} \
             impl Task { pub fn new() -> Self { Self { value: Value } } \
             fn read(&mut self, value: Value) { self.value = value; \
             let value = &self.value; self.value.open(); } } \
             impl Run for Task { fn execute(&self) {} }",
        ),
    ];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    let seed = captured_seed(&client, "Task", "src/jobs.rs").await?;
    for predicate in [
        "calls",
        "contains",
        "imports",
        "implements",
        "overrides",
        "reads",
        "writes",
        "hasType",
    ] {
        let answer = query_after_population(
            &client,
            &QueryParams {
                query: format!("ASK {{ ?source :{predicate} ?target }}"),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            answer["answer"],
            json!({"result":"boolean", "value":true}),
            "{predicate}: {answer}"
        );
    }
    let answer = query_after_population(
        &client,
        &QueryParams {
            query: format!("SELECT ?target {{ <{seed}> :calls ?target }}"),
            ..Default::default()
        },
    )
    .await?;
    assert!(answer.get("answer").is_some(), "{answer}");
    let absence = query_after_population(
        &client,
        &QueryParams {
            query: "ASK { FILTER NOT EXISTS { ?source :calls ?target } }".to_owned(),
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(
        absence["reason"],
        json!("insufficient_coverage"),
        "{absence}"
    );
    client.cancel().await?;
    task.await?;
    Ok(())
}
