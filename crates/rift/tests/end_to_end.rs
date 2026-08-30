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

use std::fs;

use harness::{
    SERIAL, StopOnDrop, TestResult, arguments, laid_out_workspace, proxied_call, proxy_client,
    require_success, run_rift,
};
use serde_json::json;

// Defect 3 - the old staging collision was the only lever that forced a
// mid-publish failure, and rollback only restored a file the syntax index
// held a previous version of. The fix stages into an exclusive tempfile per
// target and captures every target's previous bytes before the first
// publish, index membership aside.

/// A batch that fails mid-publish leaves every file byte-identical to what
/// it was, including a file the syntax index does not hold: `aaa_notes.txt`
/// publishes first, then the delete of `zzz_locked/blocked.rs` fails on a
/// read-only directory, and the already-published notes file rolls back.
#[cfg(unix)]
#[tokio::test]
async fn patch_batch_failing_mid_publish_restores_every_file() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;

    let _serial = SERIAL.lock().await;
    let directory = laid_out_workspace(
        &[
            ("aaa_notes.txt", "original notes\n"),
            ("zzz_locked/blocked.rs", "pub fn blocked() {}\n"),
        ],
        "",
    )?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let locked = root.join("zzz_locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555))?;

    let client = proxy_client(root).await?;
    let patch = "--- a/aaa_notes.txt\n+++ b/aaa_notes.txt\n@@ -1 +1 @@\n\
                 -original notes\n+changed notes\n\
                 --- a/zzz_locked/blocked.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n\
                 -pub fn blocked() {}\n";
    let call = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("patch")
                .with_arguments(arguments(&json!({ "patch": patch }))?),
        )
        .await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?;
    let refusal = call.expect_err("a directory denying write access must fail the publish");
    let rmcp::ServiceError::McpError(data) = refusal else {
        panic!("expected a protocol-level failure, got {refusal:?}");
    };
    assert_eq!(
        data.data.as_ref().and_then(|d| d.get("code")),
        Some(&json!("storage_failure"))
    );

    assert_eq!(
        fs::read_to_string(root.join("aaa_notes.txt"))?,
        "original notes\n",
        "the already-published unindexed file must roll back"
    );
    assert_eq!(
        fs::read_to_string(root.join("zzz_locked/blocked.rs"))?,
        "pub fn blocked() {}\n",
        "the file whose delete failed must be untouched"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the forced rollback")?;
    Ok(())
}

// Defect 7 - `TextFileInclusion::includes` matched `path.extension()`, which
// answers `None` for an extensionless path, so no `[search.text]` entry
// could ever reach `justfile`; a text-lane unit answered as no hit at all,
// so `docs/content/docs/index.mdx` never surfaced beside `README.md`.
//
// The design's companion proof - a ranked-only hit does not claim `content`
// - needs the live semantic tier (`RIFT_SEARCH_LIVE`, a model download from
// the hub), which is outside the gates this suite runs under and outside
// the hermetic policy every other fixture here follows; `rift-server`'s own
// `search.rs` unit tests (`search_matched_by_carries_both_members_once_the_lexical_lane_covers_text_files`
// and the sans-I/O tests beside it) prove that half of the fix directly.

/// `search` returns a `.mdx` file and a `justfile` for text they hold, with
/// each carrying `semantic: false` - the field naming a hit no syntax
/// provider claims.
#[tokio::test]
async fn search_reaches_the_mdx_file_and_the_extensionless_justfile() -> TestResult {
    let _serial = SERIAL.lock().await;
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
        &json!({ "query": "agentic development toolkit", "limit": 50 }),
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
        mdx_hits[0]["hit"]["file"]["semantic"],
        json!(false),
        "a text-lane hit no provider claims must carry semantic: false: {mdx:#}"
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
    assert_eq!(
        just_hits[0]["hit"]["file"]["semantic"],
        json!(false),
        "{just:#}"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the text-lane search")?;
    Ok(())
}

// Defect 9 - `Edit::Replace.text` advertised a bound no write tool but
// `rename_symbol` enforced, and no tool accepted a body an agent had already
// composed into a file. `BodySource::File` closes both: a bounded read
// through one absolute scratch path.

/// `patch` with `{"file": "..."}` produces a tree byte-identical to the
/// same diff sent inline.
#[tokio::test]
async fn patch_file_form_matches_the_inline_form_byte_identically() -> TestResult {
    let _serial = SERIAL.lock().await;
    let diff = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n\
                -pub fn beacon() {}\n+pub fn beacon() -> u8 { 3 }\n";

    let inline_directory = laid_out_workspace(&[("lib.rs", "pub fn beacon() {}\n")], "")?;
    let inline_root = inline_directory.path();
    let _inline_cleanup = StopOnDrop::new(inline_root);
    let inline_client = proxy_client(inline_root).await?;
    let inline_result = proxied_call(&inline_client, "patch", &json!({ "patch": diff })).await?;
    assert_eq!(
        inline_result["status"],
        json!("applied"),
        "{inline_result:#}"
    );
    inline_client.cancel().await?;
    let stopped = run_rift(inline_root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the inline patch")?;

    let scratch = tempfile::tempdir()?;
    let scratch_file = scratch.path().join("change.diff");
    fs::write(&scratch_file, diff)?;
    let file_directory = laid_out_workspace(&[("lib.rs", "pub fn beacon() {}\n")], "")?;
    let file_root = file_directory.path();
    let _file_cleanup = StopOnDrop::new(file_root);
    let file_client = proxy_client(file_root).await?;
    let file_result = proxied_call(
        &file_client,
        "patch",
        &json!({ "patch": { "file": scratch_file.to_string_lossy() } }),
    )
    .await?;
    assert_eq!(file_result["status"], json!("applied"), "{file_result:#}");
    file_client.cancel().await?;
    let stopped = run_rift(file_root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the file-form patch")?;

    assert_eq!(
        fs::read(inline_root.join("lib.rs"))?,
        fs::read(file_root.join("lib.rs"))?,
        "the inline and file forms must write byte-identical trees"
    );
    Ok(())
}

// Defect 10 - `insert_beside_anchor` (`change.rs:184-206`) spliced a `before`
// insertion at the anchor's own start byte, donating the anchor's leading
// whitespace to the new content and leaving the anchor de-indented.

/// Insert a declaration `after` an anchor, remove it, and the file's bytes
/// equal the original exactly; insert `before` an indented anchor and both
/// declarations keep their columns, with the body's own later line
/// untouched.
#[tokio::test]
async fn insert_symbol_round_trips_and_keeps_columns_before_an_indented_anchor() -> TestResult {
    let _serial = SERIAL.lock().await;
    let original = "pub fn a() {}\npub fn c() {}\n";
    let directory = laid_out_workspace(&[("lib.rs", original)], "")?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let client = proxy_client(root).await?;

    let inserted = proxied_call(
        &client,
        "insert_symbol",
        &json!({
            "anchor": "rift://symbol/rust/lib.rs/a",
            "position": "after",
            "body": "pub fn b() {}",
        }),
    )
    .await?;
    assert_eq!(inserted["status"], json!("applied"), "{inserted:#}");
    let removed = proxied_call(
        &client,
        "remove_symbol",
        &json!({ "symbol": "rift://symbol/rust/lib.rs/b", "force": true }),
    )
    .await?;
    assert_eq!(removed["status"], json!("applied"), "{removed:#}");
    assert_eq!(
        fs::read_to_string(root.join("lib.rs"))?,
        original,
        "insert after then remove must return the original bytes exactly"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the round trip")?;

    let indented =
        "impl Point {\n    pub fn zero() -> Self {\n        Point { x: 0, y: 0 }\n    }\n}\n";
    let directory = laid_out_workspace(&[("lib.rs", indented)], "")?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let client = proxy_client(root).await?;

    let body = "// c\n        // deliberately over-indented";
    let inserted = proxied_call(
        &client,
        "insert_symbol",
        &json!({
            "anchor": "rift://symbol/rust/lib.rs/Point::zero",
            "position": "before",
            "body": body,
        }),
    )
    .await?;
    assert_eq!(inserted["status"], json!("applied"), "{inserted:#}");
    assert_eq!(
        fs::read_to_string(root.join("lib.rs"))?,
        "impl Point {\n    // c\n        // deliberately over-indented\n\n    \
         pub fn zero() -> Self {\n        Point { x: 0, y: 0 }\n    }\n}\n",
        "the inserted body's first line takes the anchor's column, the anchor keeps its \
         own, and the body's own later line lands exactly as authored"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the indented insert")?;
    Ok(())
}

// Defect 14 - `command_violation` and `working_directory_violation`
// (`configuration.rs:1707-1734`) never checked `..` segments or an absolute
// path against the `ProjectPath` contract their own doc comments promised,
// so a hook ran outside the workspace.

/// A hook whose command program is absolute, and one whose `working_directory`
/// holds `..`, are each refused by the server that reads the `rift.toml`.
#[cfg(unix)]
#[tokio::test]
async fn hooks_with_an_absolute_command_or_a_dot_segment_directory_refuse() -> TestResult {
    let _serial = SERIAL.lock().await;

    let absolute_program_hook = "[[hooks]]\nid = \"escape\"\n\
        kind = \"other\"\ncommand = \"/bin/true\"\n\
        changed_paths = \"none\"\nwrites = \"none\"\nworking_directory = \"\"\nenvironment = {}\n\
        timeout = \"5s\"\noutput_limit = \"4096b\"\nfailure_severity = \"error\"\nguarantees = []\n\
        determinism = \"best_effort\"\n";
    let directory =
        laid_out_workspace(&[("lib.rs", "pub fn beacon() {}\n")], absolute_program_hook)?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let client = proxy_client(root).await?;
    let refusal = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({ "name": "beacon" }))?),
        )
        .await
        .expect_err("an absolute hook program must refuse the request");
    let rmcp::ServiceError::McpError(data) = refusal else {
        panic!("expected a protocol-level refusal, got {refusal:?}");
    };
    assert_eq!(
        data.data.as_ref().and_then(|d| d.get("code")),
        Some(&json!("configuration_invalid")),
        "{data:?}"
    );
    assert!(
        data.message.contains("command_program_absolute"),
        "{}",
        data.message
    );
    assert!(data.message.contains("hooks.command"), "{}", data.message);
    assert!(data.message.contains("/bin/true"), "{}", data.message);
    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the absolute-program refusal")?;

    let dot_segment_directory_hook = "[[hooks]]\nid = \"escape\"\n\
        kind = \"other\"\ncommand = \"true\"\n\
        changed_paths = \"none\"\nwrites = \"none\"\nworking_directory = \"../outside\"\nenvironment = {}\n\
        timeout = \"5s\"\noutput_limit = \"4096b\"\nfailure_severity = \"error\"\nguarantees = []\n\
        determinism = \"best_effort\"\n";
    let directory = laid_out_workspace(
        &[("lib.rs", "pub fn beacon() {}\n")],
        dot_segment_directory_hook,
    )?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let client = proxy_client(root).await?;
    let refusal = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({ "name": "beacon" }))?),
        )
        .await
        .expect_err("a `..`-carrying working_directory must refuse the request");
    let rmcp::ServiceError::McpError(data) = refusal else {
        panic!("expected a protocol-level refusal, got {refusal:?}");
    };
    assert_eq!(
        data.data.as_ref().and_then(|d| d.get("code")),
        Some(&json!("configuration_invalid")),
        "{data:?}"
    );
    assert!(
        data.message.contains("hook_working_directory_invalid"),
        "{}",
        data.message
    );
    assert!(data.message.contains("../outside"), "{}", data.message);
    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the working-directory refusal")?;
    Ok(())
}
