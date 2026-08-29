//! The v0.0.20 end-to-end lane: one case per defect the adversarial round
//! found, each driven through the real `rift` binary over stdio against a
//! real elected server and a real temp-directory workspace, asserting an
//! outcome a caller could observe - the bytes on disk, the exact refusal,
//! the hits an answer carries, the warning it names.
//!
//! Every case reaches the shared harness in `harness.rs` - `laid_out_workspace`,
//! `proxy_client`, `proxied_call`, `live_engine_gate::engine_live` - the same
//! entry points `mcp_proxy.rs` uses. `raw_proxy.rs` adds the one capability
//! that harness lacks: a session over raw stdio pipes, for the one case that
//! must send a frame rmcp's own client cannot construct.

mod engine_fixture;
mod harness;
mod live_engine_gate;
mod raw_proxy;
mod rust_engine;

use std::fs;

use harness::{
    RUST_PROJECT_ROOT, SERIAL, StopOnDrop, TestResult, arguments, laid_out_workspace, proxied_call,
    proxied_engine_call, proxy_client, require_success, run_rift, rust_engine_workspace,
    rust_project, workspace,
};
use raw_proxy::RawProxySession;
use serde_json::json;

/// The SHA-256 of the empty byte range's first eight hex characters: the
/// witness a clamped range collapses to, and the one a forged out-of-bounds
/// [`rift_protocol::read::NodeId`] carries.
const EMPTY_RANGE_WITNESS: &str = "e3b0c442";

// Defect 1 - `crates/rift-server/src/change.rs:562` staged every rewrite at
// `absolute.with_extension("rift-staged")`, so two files sharing a stem in one
// directory staged to the same path and one lost its bytes permanently.

/// A patch touching `report.rs` and `report.log` in one directory leaves
/// both holding their own bytes: no staged-path collision between two files
/// that share a stem.
#[tokio::test]
async fn patch_writes_two_files_sharing_a_stem_without_collision() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = laid_out_workspace(
        &[
            ("report.rs", "pub fn one() {}\n"),
            ("report.log", "original log content\n"),
        ],
        "",
    )?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    let patch = "--- a/report.log\n+++ b/report.log\n@@ -1 +1 @@\n\
                 -original log content\n+NEW LOG CONTENT\n\
                 --- a/report.rs\n+++ b/report.rs\n@@ -1 +1 @@\n\
                 -pub fn one() {}\n+pub fn two() {}\n";
    let structured = proxied_call(&client, "patch", &json!({ "patch": patch })).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");

    assert_eq!(
        fs::read_to_string(root.join("report.rs"))?,
        "pub fn two() {}\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("report.log"))?,
        "NEW LOG CONTENT\n",
        "the unindexed sibling keeps its own bytes, not report.rs's"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the two-file patch")?;
    Ok(())
}

// Defect 2 - `fs::rename` onto a symlinked path (`change.rs:575,728`) replaced
// the link with a regular file, and a link resolving outside the workspace
// published anyway.

/// A patch through an in-workspace symlink leaves the link a link and
/// updates its resolved target; a link pointing outside the workspace
/// refuses and leaves both files untouched.
#[cfg(unix)]
#[tokio::test]
async fn patch_through_symlink_updates_target_and_refuses_outside_workspace() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = laid_out_workspace(&[("real.rs", "pub fn real() {}\n")], "")?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    std::os::unix::fs::symlink("real.rs", root.join("link_inside.rs"))?;

    let outside = tempfile::tempdir()?;
    fs::write(outside.path().join("secret.rs"), "pub fn outside() {}\n")?;
    std::os::unix::fs::symlink(
        outside.path().join("secret.rs"),
        root.join("link_outside.rs"),
    )?;

    let client = proxy_client(root).await?;

    let inside_patch = "--- a/link_inside.rs\n+++ b/link_inside.rs\n@@ -1 +1 @@\n\
                         -pub fn real() {}\n+pub fn renamed() {}\n";
    let structured = proxied_call(&client, "patch", &json!({ "patch": inside_patch })).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert!(
        structured["summary"]["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("resolved target real.rs"),
        "{structured:#}"
    );
    assert!(
        fs::symlink_metadata(root.join("link_inside.rs"))?
            .file_type()
            .is_symlink(),
        "the link must remain a link"
    );
    assert_eq!(
        fs::read_to_string(root.join("real.rs"))?,
        "pub fn renamed() {}\n"
    );

    let outside_patch = "--- a/link_outside.rs\n+++ b/link_outside.rs\n@@ -1 +1 @@\n\
                          -pub fn outside() {}\n+pub fn renamed2() {}\n";
    let structured = proxied_call(&client, "patch", &json!({ "patch": outside_patch })).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unsupported"), "{structured:#}");
    assert!(
        structured["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("outside the workspace"),
        "{structured:#}"
    );
    assert!(
        fs::symlink_metadata(root.join("link_outside.rs"))?
            .file_type()
            .is_symlink(),
        "a refused publish must leave the link untouched"
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("secret.rs"))?,
        "pub fn outside() {}\n",
        "the out-of-workspace target must be untouched"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the symlink patches")?;
    Ok(())
}

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

// Defect 4 - `resolve_create` (`patch.rs:452-466`), `insert_at_file`
// (`change.rs:214-252`), and `move_file`'s destination check
// (`move_file.rs:242`) each reached the filesystem without consulting
// `[source]`.

/// `patch` creating into an excluded directory, `insert_symbol` with a file
/// target in one, and `move_file` into one each refuse and name the
/// `[source]` policy.
#[tokio::test]
async fn write_paths_refuse_an_excluded_source_destination() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = laid_out_workspace(
        &[
            ("hidden/secret.rs", "pub fn kept_secret() {}\n"),
            ("visible/mover.rs", "pub fn mover() {}\n"),
        ],
        "[source]\nexclude = [\"hidden/**\"]\n",
    )?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let client = proxy_client(root).await?;

    let create_patch = "--- /dev/null\n+++ b/hidden/new.rs\n@@ -0,0 +1 @@\n+pub fn injected() {}\n";
    let created = proxied_call(&client, "patch", &json!({ "patch": create_patch })).await?;
    assert_eq!(created["status"], json!("refused"), "{created:#}");
    assert_eq!(created["reason"], json!("unsupported"), "{created:#}");
    assert!(
        created["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("[source]"),
        "{created:#}"
    );
    assert!(!root.join("hidden/new.rs").exists());

    let inserted = proxied_call(
        &client,
        "insert_symbol",
        &json!({
            "file": "hidden/secret.rs",
            "position": "after",
            "body": "pub fn injected2() {}",
            "create_missing": false,
        }),
    )
    .await?;
    assert_eq!(inserted["status"], json!("refused"), "{inserted:#}");
    assert!(
        inserted["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("[source]"),
        "{inserted:#}"
    );
    assert_eq!(
        fs::read_to_string(root.join("hidden/secret.rs"))?,
        "pub fn kept_secret() {}\n",
        "an excluded file target must be untouched"
    );

    let moved = proxied_call(
        &client,
        "move_file",
        &json!({ "from": "visible/mover.rs", "to": "hidden/mover.rs" }),
    )
    .await?;
    assert_eq!(moved["status"], json!("refused"), "{moved:#}");
    assert!(
        moved["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("[source]"),
        "{moved:#}"
    );
    assert!(
        root.join("visible/mover.rs").exists(),
        "a refused move leaves the source in place"
    );
    assert!(!root.join("hidden/mover.rs").exists());

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the excluded-destination refusals")?;
    Ok(())
}

// Defect 5 - three call sites in `rift-index/src/workspace.rs` propagated one
// file's UTF-8 decode failure through `?`, aborting the whole index build so
// every later call against the workspace timed out.

/// A workspace holding one file whose bytes are not UTF-8 still serves
/// every other file, the answer names the skipped one, and addressing it
/// refuses `content_unavailable`.
#[tokio::test]
async fn non_utf8_file_is_skipped_with_a_warning_and_refuses_content_unavailable() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = laid_out_workspace(&[("lib.rs", "pub fn beacon() {}\n")], "")?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    fs::write(
        root.join("bad.rs"),
        [b'o', b'n', b'e', b'\n', b't', b'w', b'o', 0xFF, 0xFE, b'\n'],
    )?;

    let client = proxy_client(root).await?;
    let lookup = proxied_call(&client, "get_symbol", &json!({ "name": "beacon" })).await?;
    assert_eq!(
        lookup["hits"][0]["symbol"]["name"],
        json!("beacon"),
        "an unrelated file must still be served: {lookup:#}"
    );
    let warnings = lookup["warnings"]
        .as_array()
        .expect("warnings must be an array");
    assert!(
        warnings.iter().any(|warning| {
            warning["code"] == json!("source_unavailable")
                && warning["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("bad.rs")
        }),
        "the answer must name the skipped file: {lookup:#}"
    );

    let refusal = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("nodes")
                .with_arguments(arguments(&json!({ "path": "bad.rs", "position": 0 }))?),
        )
        .await
        .expect_err("addressing the invalid file must refuse");
    let rmcp::ServiceError::McpError(data) = refusal else {
        panic!("expected a protocol-level refusal, got {refusal:?}");
    };
    assert_eq!(
        data.data.as_ref().and_then(|d| d.get("code")),
        Some(&json!("content_unavailable")),
        "{data:?}"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the non-UTF-8 workspace")?;
    Ok(())
}

// Defect 6 - `node_witness` (`read.rs:923-931`) clamped both offsets to
// `source.len()` before hashing, so a range wholly past the end of the file
// hashed the empty string and a forged address verified.

/// A forged out-of-bounds `NodeId` refuses identically on `remove_node` and
/// `replace_node`, and nothing on disk changes.
#[tokio::test]
async fn forged_out_of_bounds_node_id_refuses_identically_on_remove_and_replace() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = laid_out_workspace(&[("lib.rs", "pub fn beacon() {}\n")], "")?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let client = proxy_client(root).await?;

    let node = format!("rift://node/rust/lib.rs@9999-10050#{EMPTY_RANGE_WITNESS}");

    let remove_refusal = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("remove_node")
                .with_arguments(arguments(&json!({ "node": node }))?),
        )
        .await
        .expect_err("an out-of-bounds node address must refuse remove_node");
    let replace_refusal = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("replace_node").with_arguments(arguments(
                &json!({ "node": node, "body": "should not appear" }),
            )?),
        )
        .await
        .expect_err("the identical address must refuse replace_node too");

    let (rmcp::ServiceError::McpError(remove_data), rmcp::ServiceError::McpError(replace_data)) =
        (remove_refusal, replace_refusal)
    else {
        panic!("expected protocol-level refusals from both tools");
    };
    assert_eq!(
        remove_data.data, replace_data.data,
        "both tools must refuse identically"
    );
    assert_eq!(
        remove_data.data.as_ref().and_then(|d| d.get("code")),
        Some(&json!("invalid_request"))
    );
    assert!(
        remove_data
            .message
            .contains("range outside the addressed file"),
        "{}",
        remove_data.message
    );
    assert_eq!(
        fs::read_to_string(root.join("lib.rs"))?,
        "pub fn beacon() {}\n",
        "neither refusal may touch the file"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the forged-address refusals")?;
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

// Defect 8 - `fetch_limit = limit * SEARCH_OVERFETCH_FACTOR` tied the
// candidate pool to the requested page size, so a small `limit` starved the
// ranked lane and `total_pages` moved with it.

/// Paging one workspace at `limit: 1` and at `limit: 50` reconstructs the
/// same ordered pool with the same `total_pages` derived from the pool, not
/// the page size.
#[tokio::test]
async fn search_paging_reconstructs_the_same_pool_at_every_limit() -> TestResult {
    let _serial = SERIAL.lock().await;
    let files: Vec<(&str, &str)> = vec![
        ("mod_1.rs", "pub fn helper_1() {}\n"),
        ("mod_2.rs", "pub fn helper_2() {}\n"),
        ("mod_3.rs", "pub fn helper_3() {}\n"),
        ("mod_4.rs", "pub fn helper_4() {}\n"),
        ("mod_5.rs", "pub fn helper_5() {}\n"),
    ];
    let directory = laid_out_workspace(&files, "")?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let client = proxy_client(root).await?;

    let wide = proxied_call(
        &client,
        "search",
        &json!({ "query": "helper", "limit": 50 }),
    )
    .await?;
    assert_eq!(wide["pagination"]["total_pages"], json!(1), "{wide:#}");
    let wide_identities: Vec<serde_json::Value> = wide["results"]
        .as_array()
        .expect("results must be an array")
        .iter()
        .map(|result| result["path"].clone())
        .collect();

    let mut narrow_identities = Vec::new();
    let mut total_pages = None;
    for page_index in 0.. {
        let page = proxied_call(
            &client,
            "search",
            &json!({ "query": "helper", "limit": 1, "page_index": page_index }),
        )
        .await?;
        let this_total = page["pagination"]["total_pages"]
            .as_u64()
            .expect("total_pages must be a number");
        total_pages = Some(this_total);
        let hits = page["results"]
            .as_array()
            .expect("results must be an array");
        if hits.is_empty() {
            break;
        }
        narrow_identities.extend(hits.iter().map(|result| result["path"].clone()));
        if page_index + 1 >= this_total {
            break;
        }
    }
    assert_eq!(
        total_pages,
        Some(wide_identities.len() as u64),
        "limit: 1 must report as many pages as the pool holds hits"
    );
    assert_eq!(
        narrow_identities, wide_identities,
        "paging at limit: 1 must reconstruct the same ordered pool limit: 50 returns in one page"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the paging comparison")?;
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

// Defect 11 - `declined_refusal` (`rename.rs:993`) reused `target_exists`,
// asserting a declaration the engine plainly resolved does not exist.
//
// Not landed in this tree yet: `orphan_fn` sits outside the crate graph
// rust-analyzer resolves, so the engine declines the rename. Against the
// settled design the refusal carries `engine_proposed_edits`,
// `expected: true, observed: false`, and no `target_exists` precondition;
// this assertion fails today against the still-unfixed `declined_refusal`
// and is expected to pass once that fix lands.

/// A rename whose engine declines names the engine's decline, not a
/// missing target.
#[tokio::test]
async fn rename_declined_by_engine_names_the_decline_not_a_missing_target() -> TestResult {
    let _serial = SERIAL.lock().await;
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let mut files = rust_project();
    files.push(("orphan.rs", "pub fn orphan_fn() {}\n"));
    let directory = laid_out_workspace(&files, &rust_engine::rust_engine_configuration())?;
    let root = directory.path();
    rust_engine::require_rust_analyzer(root);
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    let structured = proxied_engine_call(
        &client,
        "rename_symbol",
        &json!({ "symbol": "rift://symbol/rust/orphan.rs/orphan_fn", "new_name": "renamed" }),
    )
    .await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    let preconditions = structured["preconditions"]
        .as_array()
        .expect("preconditions must be an array");
    assert!(
        preconditions
            .iter()
            .all(|precondition| precondition["kind"] != json!("target_exists")),
        "an engine's decline must never claim the target does not exist: {structured:#}"
    );
    assert!(
        preconditions.iter().any(|precondition| {
            precondition["kind"] == json!("engine_proposed_edits")
                && precondition["expected"] == json!({ "kind": "boolean", "value": true })
                && precondition["observed"] == json!({ "kind": "boolean", "value": false })
        }),
        "the refusal must name the engine's own decline: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(root.join("orphan.rs"))?,
        "pub fn orphan_fn() {}\n",
        "a declined rename must leave the file untouched"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the declined rename")?;
    Ok(())
}

// Defect 12 - `move_file`'s `AnsweredNothing` warning only fired while the
// engine had never announced any work; once it had, an empty will-rename
// answer was trusted as "nothing needs updating" even where Rust's module
// system has no way to express the move at all.

/// A `move_file` that breaks the module graph - nesting the file where no
/// `mod` declaration without `#[path]` can reach it - carries
/// `references_not_updated`.
#[tokio::test]
async fn move_file_breaking_the_module_graph_carries_references_not_updated() -> TestResult {
    let _serial = SERIAL.lock().await;
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let directory = rust_engine_workspace()?;
    let root = directory.path();
    rust_engine::require_rust_analyzer(root);
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    let structured = proxied_engine_call(
        &client,
        "move_file",
        &json!({ "from": "hub.rs", "to": "deep/nested/dir/hub.rs" }),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warned = structured["summary"]["diagnostics"]
        .as_array()
        .is_some_and(|findings| {
            findings
                .iter()
                .any(|finding| finding["code"] == json!("rift.move.references_not_updated"))
        });
    assert!(
        warned,
        "a move no non-#[path] mod declaration can express must carry the warning: {structured:#}"
    );
    assert!(!root.join("hub.rs").exists());
    assert!(root.join("deep/nested/dir/hub.rs").exists());
    assert_eq!(
        fs::read_to_string(root.join("lib.rs"))?,
        RUST_PROJECT_ROOT,
        "no engine edit updates a declaration the module system cannot express: lib.rs is \
         untouched"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the mod-graph-breaking move")?;
    Ok(())
}

// Defect 13 - the stdio transport's own line codec dropped a frame that
// failed to parse as JSON instead of answering JSON-RPC 2.0's mandatory
// `-32700`, silently, with no signal the caller's request was ever lost.

/// An invalid JSON frame is answered `-32700` and the next valid frame on
/// the same connection still runs.
#[tokio::test]
async fn invalid_json_frame_answers_parse_error_and_the_next_frame_still_runs() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let mut session = RawProxySession::connect(root).await?;
    session.send_line("not valid json at all").await?;
    let response = session.read_response().await?;
    assert_eq!(response["error"]["code"], json!(-32700), "{response}");
    assert_eq!(response["id"], serde_json::Value::Null, "{response}");

    session
        .send_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .await?;
    let follow_up = session.read_response().await?;
    assert_eq!(follow_up["id"], json!(2), "{follow_up}");
    assert!(follow_up.get("error").is_none(), "{follow_up}");
    let listed = follow_up["result"]["tools"]
        .as_array()
        .expect("tools/list must answer an array");
    assert!(
        !listed.is_empty(),
        "the next frame must genuinely run: {follow_up}"
    );

    session.end().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the malformed-frame session")?;
    Ok(())
}

// Defect 14 - `command_violation` and `working_directory_violation`
// (`configuration.rs:1707-1734`) never checked `..` segments or an absolute
// path against the `ProjectPath` contract their own doc comments promised,
// so a hook ran outside the workspace.

/// A hook whose `program` is absolute, and one whose `working_directory`
/// holds `..`, are each refused by the server that reads the `rift.toml`.
#[cfg(unix)]
#[tokio::test]
async fn hooks_with_an_absolute_program_or_a_dot_segment_directory_refuse() -> TestResult {
    let _serial = SERIAL.lock().await;

    let absolute_program_hook = "[[hooks]]\ntype = \"command\"\nid = \"escape\"\n\
        kind = \"other\"\nprogram = \"/bin/true\"\narguments = []\n\
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
        data.message.contains("hook_executable_absolute"),
        "{}",
        data.message
    );
    assert!(data.message.contains("escape"), "{}", data.message);
    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the absolute-program refusal")?;

    let dot_segment_directory_hook = "[[hooks]]\ntype = \"command\"\nid = \"escape\"\n\
        kind = \"other\"\nprogram = \"true\"\narguments = []\n\
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

#[test]
fn rust_engine_fixture_pins_1_98_and_bounded_retries() {
    let fixture = rust_engine::fixture();
    assert_eq!(fixture.program, "rustup");
    assert_eq!(fixture.arguments, ["run", "1.98", "rust-analyzer"]);
    assert_eq!(
        fixture.extra_toml,
        "\n[engines.rust.retry]\nattempts = 16\n"
    );
}
