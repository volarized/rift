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

use harness::{
    StopOnDrop, TestResult, laid_out_workspace, proxied_call, proxy_client, require_success,
    run_rift,
};
use serde_json::json;

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

/// File-target `search` returns a `.mdx` file and a `justfile` for text they hold, each
/// as a flat file hit whose `matched_by` claims the `content` lane.
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
        "",
    )?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let client = proxy_client(root).await?;

    let mdx = proxied_call(
        &client,
        "search",
        &json!({ "query": "agentic development toolkit", "target": "file", "limit": 50 }),
    )
    .await?;
    let mdx_hits: Vec<&serde_json::Value> = mdx["results"]
        .as_array()
        .expect("results must be an array")
        .iter()
        .filter(|result| result["path"] == json!("docs/content/docs/index.mdx"))
        .collect();
    assert_eq!(mdx_hits.len(), 1, "the mdx file must surface once: {mdx:#}");
    assert_eq!(
        mdx_hits[0]["hit"]["target"],
        json!("file"),
        "a text-lane hit answers as a file target: {mdx:#}"
    );
    assert!(
        mdx_hits[0]["matched_by"]
            .as_array()
            .is_some_and(|lanes| lanes.contains(&json!("content"))),
        "a text-lane hit claims the content lane: {mdx:#}"
    );

    let just = proxied_call(
        &client,
        "search",
        &json!({ "query": "cargo fmt --all --check", "limit": 50 }),
    )
    .await?;
    let just_hits: Vec<&serde_json::Value> = just["results"]
        .as_array()
        .expect("results must be an array")
        .iter()
        .filter(|result| result["path"] == json!("justfile"))
        .collect();
    assert_eq!(
        just_hits.len(),
        1,
        "the extensionless justfile must surface: {just:#}"
    );
    assert_eq!(just_hits[0]["hit"]["target"], json!("file"), "{just:#}");
    assert!(
        just_hits[0]["matched_by"]
            .as_array()
            .is_some_and(|lanes| lanes.contains(&json!("content"))),
        "{just:#}"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the text-lane search")?;
    Ok(())
}
