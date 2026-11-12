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
/// initial write behind that publication, so a search can still return identifier matches
/// with `lexical_ranking_unavailable` or `stale_index` while the store catches up.
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
        let population_pending =
            answer["warnings"]
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
