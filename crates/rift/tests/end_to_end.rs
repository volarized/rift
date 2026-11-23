//! The v0.0.20 end-to-end lane: one case per defect the adversarial round
//! found, each driven through the real `rift` binary over stdio against a
//! real elected server and a real temp-directory workspace, asserting an
//! outcome a caller could observe - the bytes on disk, the exact refusal,
//! the hits an answer carries, the warning it names.
//!
//! Every case reaches the shared harness in `harness.rs` - `laid_out_workspace`,
//! `proxy_client`, `proxied_call` - the same entry points `mcp_proxy.rs` uses.

#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod engine_fixture;
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod harness;
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod rust_engine;
#[allow(
    dead_code,
    reason = "shared test identity helper, used by sibling suites"
)]
mod test_case;

use harness::{
    FailureWindow, StopOnDrop, TestResult, assigned_port_key, await_workspace_ready,
    laid_out_workspace, proxied_call, proxy_client, require_success, run_rift, within,
};
use serde_json::json;
use std::time::Duration;

/// Calls `search` until the lexical population pass has landed.
///
/// Workspace map readiness covers local file preparation. The lexical lane commits its
/// initial write behind that publication, so a search can return `index_preparing` or
/// identifier matches with `lexical_ranking_unavailable` or `stale_index` while the store catches up.
///
/// # Errors
///
/// Returns an error if the store does not rank the answer after 60 polling delays and up
/// to 60 follow-up requests. The 50 ms delays total at most three seconds; request time
/// is additional. Each proxied call allows up to eight attempts, each with a 45-second
/// timeout.
async fn search_after_population(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    arguments: &serde_json::Value,
) -> TestResult<serde_json::Value> {
    const SEARCH_TIER_ATTEMPTS_MAX: usize = 60;
    const SEARCH_TIER_POLL: Duration = Duration::from_millis(50);

    let query = arguments["query"].as_str().unwrap_or("<missing query>");
    let mut answer = proxied_call(client, "search", arguments).await?;
    for _attempt in 0..SEARCH_TIER_ATTEMPTS_MAX {
        let population_pending = answer["reason"] == "index_preparing"
            || answer["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|warning| {
                    matches!(
                        warning["code"].as_str(),
                        Some("lexical_ranking_unavailable" | "stale_index")
                    )
                });
        if !population_pending {
            return Ok(answer);
        }
        tokio::time::sleep(SEARCH_TIER_POLL).await;
        answer = proxied_call(client, "search", arguments).await?;
    }
    Err(format!(
        "the population lane never stamped the served tree for query {query}; the last answer was {answer:#}"
    )
    .into())
}

// Defect 7 - `TextFileInclusion::includes` matched `path.extension()`, which
// answers `None` for an extensionless path, so no `[search.text]` entry
// could ever reach `justfile`; a text-lane unit answered as no hit at all,
// so `docs/content/docs/index.mdx` never surfaced beside `README.md`.
//
// The design's companion proof - a ranked-only hit does not claim `content`
// - needs the live vector ranking (`RIFT_LIVE_SEARCH`, a model download from
// the hub), which is outside the gates this suite runs under and outside
// the hermetic policy every other fixture here follows; `rift-server`'s own
// `search.rs` unit tests (`search_matched_by_carries_both_members_once_the_lexical_lane_covers_text_files`
// and the sans-I/O tests beside it) prove that half of the fix directly.

/// Text-file retrieval keeps the original file target and content match through SELECT.
#[tokio::test]
async fn search_reaches_the_mdx_file_and_the_extensionless_justfile() -> TestResult {
    let directory = laid_out_workspace(
        &[
            (
                "README.md",
                "# agentic development toolkit\n\nThis is the readme.\n",
            ),
            (
                "docs/content/docs/index.mdx",
                "# agentic development toolkit\n\nSame phrase in mdx.\n",
            ),
            ("justfile", "build:\n\tcargo fmt --all --check\n"),
        ],
        &assigned_port_key()?,
    )?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let client = proxy_client(root).await?;
    within("workspace preparation", await_workspace_ready(&client)).await??;
    for (query, path) in [
        (
            r#"SELECT ?path ?target ?matched_by {
          SERVICE :search {
            ?hit :query "agentic development toolkit"; :target "file";
              :path ?path; :hit ?record; :matched_by ?lane .
            ?record :target ?target . ?lane :value ?matched_by .
            FILTER(?matched_by = "content")
          }
        } LIMIT 50"#,
            "docs/content/docs/index.mdx",
        ),
        (
            r#"SELECT ?path ?target ?matched_by {
          SERVICE :search {
            ?hit :query "cargo fmt --all --check"; :path ?path;
              :hit ?record; :matched_by ?lane .
            ?record :target ?target . ?lane :value ?matched_by .
            FILTER(?matched_by = "content")
          }
        } LIMIT 50"#,
            "justfile",
        ),
    ] {
        let answer = search_after_population(&client, &json!({"query":query})).await?;
        let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } =
            serde_json::from_value(answer["answer"].clone())?
        else {
            return Err("file retrieval must return SELECT bindings".into());
        };
        let hits = bindings
            .iter()
            .filter(|row| {
                matches!(row.get("path"),
            Some(rift_protocol::query::RdfTerm::Literal { value, .. }) if value == path)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            hits.len(),
            1,
            "one original content hit for {path}: {answer}"
        );
        assert!(
            matches!(hits[0].get("target"), Some(rift_protocol::query::RdfTerm::Literal { value, .. }) if value == "file"),
            "text retrieval keeps file target: {answer}"
        );
        assert!(
            matches!(hits[0].get("matched_by"), Some(rift_protocol::query::RdfTerm::Literal { value, .. }) if value == "content"),
            "text retrieval keeps content match: {answer}"
        );
    }
    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the text-lane search")?;
    failure_window.passed();
    Ok(())
}

const DECLARATION_READS: &[(&str, &str, Option<&str>)] = &[
    (
        "rust/rift_live_fixture/Point/x",
        "field_declaration",
        Some("rust/rift_live_fixture/Point"),
    ),
    ("rust/rift_live_fixture/Bits", "union_item", None),
    (
        "rust/rift_live_fixture/Shape/Unit",
        "associated_type",
        Some("rust/rift_live_fixture/Shape"),
    ),
    (
        "rust/rift_live_fixture/a/inner",
        "function",
        Some("rust/rift_live_fixture/a"),
    ),
    (
        "rust/rift_live_fixture/b/inner",
        "function",
        Some("rust/rift_live_fixture/b"),
    ),
    ("rust/rift_live_fixture/Round", "export", None),
    (
        "javascript/.%2Fmain.js/outer/local",
        "variable",
        Some("javascript/.%2Fmain.js/outer"),
    ),
    (
        "javascript/.%2Fmain.js/obj/method",
        "method",
        Some("javascript/.%2Fmain.js/obj"),
    ),
    (
        "javascript/.%2Fmain.js/default/anon",
        "method",
        Some("javascript/.%2Fmain.js/default"),
    ),
    (
        "javascript/.%2Fmain.js/Widget/count",
        "property",
        Some("javascript/.%2Fmain.js/Widget"),
    ),
    ("javascript/.%2Fmain.js/major", "variable", None),
    ("javascript/.%2Fmain.js/mi", "variable", None),
    ("python/pkg", "module", None),
    ("python/pkg/Model", "export", None),
    ("python/pkg/models/A", "variable", None),
    ("python/pkg/models/B", "variable", None),
    ("python/pkg/models/Pair", "type_alias", None),
    ("python/pkg/models/FLAG", "variable", None),
    (
        "python/pkg/models/Model/save",
        "method",
        Some("python/pkg/models/Model"),
    ),
    (
        "python/pkg/models/Model/name",
        "variable",
        Some("python/pkg/models/Model"),
    ),
];

/// Declaration facts retain their containers and public import paths through the CLI.
///
/// The one served fixture here that keeps the shipped `[dependencies]` table, so each
/// native runner starts a server over its own `rustc`, Node.js, and Python.
#[tokio::test]
async fn declaration_members_bound_names_and_module_exports_are_addressable() -> TestResult {
    let directory = harness::bounded_workspace(
        &[
            ("Cargo.toml", harness::RUST_PROJECT_MANIFEST),
            (
                "lib.rs",
                "pub struct Point {\n    /// Horizontal.\n    pub x: i32,\n}\npub union Bits { pub i: u32 }\npub trait Shape { type Unit; fn area(&self); }\nimpl Shape for Point { type Unit = i32; fn area(&self) {} }\npub fn a() { fn inner() {} }\npub fn b() { fn inner() {} }\nmod shapes { pub struct Circle; }\npub use shapes::Circle as Round;\n",
            ),
            (
                "main.js",
                "export function outer() { const local = () => 1; }\nconst obj = { method() {} };\nexport default class { anon() {} }\nexport class Widget { count = 0; }\nexport const { major, minor: mi } = version;\n",
            ),
            (
                "pyproject.toml",
                "[build-system]\nrequires = ['pdm-backend']\nbuild-backend = 'pdm.backend'\n",
            ),
            (
                "pkg/__init__.py",
                "\"\"\"Package documentation.\"\"\"\nfrom .models import Model as Model\n",
            ),
            (
                "pkg/models.py",
                "A, B = 1, 2\ntype Pair = tuple[int, int]\nif True:\n    FLAG = 1\nclass Model:\n    def __init__(self):\n        self.name = 'x'\n    def save(self):\n        pass\n",
            ),
        ],
        "",
        &assigned_port_key()?,
    )?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let client = proxy_client(root).await?;
    within("workspace preparation", await_workspace_ready(&client)).await??;
    for &(path, kind, container) in DECLARATION_READS {
        let id = format!("rift://symbol/local/{path}");
        let answer = proxied_call(
            &client,
            "get_symbol",
            &json!({"id":id, "page":1, "max_results":10}),
        )
        .await?;
        assert_eq!(answer["symbol"]["id"], id, "{answer}");
        assert_eq!(answer["symbol"]["kind"], kind, "{answer}");
        let expected = container.map(|path| format!("rift://symbol/local/{path}"));
        assert_eq!(
            answer["symbol"]["container"].as_str(),
            expected.as_deref(),
            "{answer}"
        );
        if kind != "export" {
            assert_eq!(
                answer["declarations"].as_array().map(Vec::len),
                Some(1),
                "{answer}"
            );
        }
    }
    declaration_pages(&client).await?;
    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after declaration reads")?;
    failure_window.passed();
    Ok(())
}

/// Newly collected declarations use the same paging contract for symbols and facets.
async fn declaration_pages(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult {
    let id = "rift://symbol/local/rust/rift_live_fixture/Point/x";
    let request = json!({"id":id, "page":1, "max_results":1});
    let symbol = proxied_call(client, "get_symbol", &request).await?;
    let revision = symbol["revision"].as_str().ok_or("symbol revision")?;
    assert_eq!(
        symbol["pagination"],
        json!({"page":1,"pages":1,"max_results":1,"total":1})
    );
    assert!(
        symbol["symbol"]["kinds"]
            .as_array()
            .ok_or("symbol kinds")?
            .contains(&json!("member"))
    );
    let request = json!({"symbol":id,"revision":revision,"page":1,"max_results":1});
    let facets = proxied_call(client, "facets", &request).await?;
    assert_eq!(facets["pagination"], symbol["pagination"]);
    assert_eq!(facets["revision"], revision);
    assert_eq!(facets["declarations"][0]["kind"], "field_declaration");
    for (tool, selector) in [("get_symbol", "id"), ("facets", "symbol")] {
        let request = json!({selector:id,"revision":revision,"page":2,"max_results":1});
        let answer = proxied_call(client, tool, &request).await?;
        assert_eq!(answer["declarations"], json!([]), "{answer}");
        assert_eq!(
            answer["pagination"],
            json!({"page":2,"pages":1,"max_results":1,"total":1})
        );
        assert_eq!(answer["revision"], revision);
    }
    Ok(())
}
