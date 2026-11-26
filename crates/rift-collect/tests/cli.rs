//! Runner entry point controls.

#![cfg(feature = "runtime")]

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

fn command() -> Command {
    Command::new(
        std::env::var_os("CARGO_BIN_EXE_rift-collect")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_rift-collect").into()),
    )
}

async fn run(mut command: Command) -> Output {
    command.kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("runner exits before deadline")
        .expect("runner starts")
}

fn manifest(root: &Path, revision: &str) -> std::path::PathBuf {
    let path = root.join("sources.json");
    std::fs::write(root.join("entry.py"), "f()").expect("fixture source");
    std::fs::write(
        &path,
        json!({
            "producer": "fixture.calls",
            "revision": revision,
            "context": "c".repeat(64),
            "category": "calls",
            "sources": [{
                "unit": "rift://source/project/entry.py",
                "path": "entry.py",
                "filename": "entry.py"
            }]
        })
        .to_string(),
    )
    .expect("fixture manifest");
    path
}

fn collect(root: &Path, manifest: &Path) -> Command {
    let mut command = command();
    command
        .arg("collect")
        .arg(manifest)
        .arg(root.join("checkpoints.sqlite"))
        .arg("--");
    command
}

#[tokio::test]
async fn schema_and_manifest_failures_are_visible_at_the_cli() {
    let mut schema = command();
    schema.arg("schema");
    let output = run(schema).await;
    assert!(output.status.success());
    let schema: Value = serde_json::from_slice(&output.stdout).expect("schema");
    assert_eq!(schema["version"], 1);
    assert!(schema["extract_result"]["properties"]["observations"].is_object());

    let empty = run(command()).await;
    assert_eq!(empty.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&empty.stderr).contains("usage: rift-collect"));

    let directory = tempfile::tempdir().expect("fixture directory");
    let path = manifest(directory.path(), "invalid");
    let mut invalid = collect(directory.path(), &path);
    invalid.arg("unused");
    let result = run(invalid).await;
    assert_eq!(result.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("expected a lowercase SHA-256 digest")
    );
}

#[cfg(unix)]
fn host(root: &Path, manifest: &Path, coverage: &str, after_response: &str) -> Command {
    let mut command = collect(root, manifest);
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "result": {
            "coverage": coverage,
            "observations": [{
                "id": "fixture", "occurrence": { "start": 0, "end": 3 },
                "enclosing": null, "target": null, "evidence": "syntax"
            }]
        }
    })
    .to_string();
    command.args([
        "sh",
        "-c",
        r#"printf 'Content-Length: %s\r\n\r\n%s' "${#1}" "$1"; case "$2" in fail) cat >/dev/null; exit 3;; esac; exec cat >/dev/null"#,
        "fixture",
        &body,
        after_response,
    ]);
    command
}

#[cfg(unix)]
#[tokio::test]
async fn runner_streams_a_batch_and_skips_host_start_on_warm_restart() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let root = directory.path();
    let path = manifest(root, &"a".repeat(64));
    let cold = run(host(root, &path, "complete", "")).await;
    assert!(cold.status.success(), "{:?}", cold.stderr);
    assert!(String::from_utf8_lossy(&cold.stderr).contains("reused=0 collected=1 partial=0"));
    let batch: Value = serde_json::from_slice(&cold.stdout).expect("batch");
    assert_eq!(batch["batch"]["observations"][0]["id"], "fixture");
    assert_eq!(batch["producer"], "fixture.calls");

    let mut missing = collect(root, &path);
    missing.arg(root.join("missing-collector"));
    let warm = run(missing).await;
    assert!(warm.status.success());
    assert_eq!(warm.stdout, cold.stdout);
    assert!(String::from_utf8_lossy(&warm.stderr).contains("reused=1 collected=0 partial=0"));

    std::fs::write(root.join("entry.py"), "g()").expect("edit source");
    let edited = run(host(root, &path, "complete", "")).await;
    assert!(edited.status.success());
    assert_ne!(edited.stdout, cold.stdout);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("reused=0 collected=1 partial=0"));
}

#[cfg(unix)]
#[tokio::test]
async fn partial_and_nonzero_host_exit_keep_distinct_status() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let root = directory.path();
    let path = manifest(root, &"a".repeat(64));
    let partial = run(host(root, &path, "partial", "")).await;
    assert_eq!(partial.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&partial.stderr).contains("reused=0 collected=0 partial=1"));

    let failed = run(host(root, &path, "complete", "fail")).await;
    assert_eq!(failed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("collector exited with"));
}

#[tokio::test]
async fn jvm_cli_uses_separate_checkpoints_for_syntax_and_lsp() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Entry.java");
    let manifest = root.path().join("sources.json");
    let cache = root.path().join("checkpoints.sqlite");
    std::fs::write(&source, "class Entry { int call() { return helper(); } }").expect("source");
    std::fs::write(
        &manifest,
        json!({
            "producer": "fixture.jvm",
            "revision": "a".repeat(64),
            "context": "b".repeat(64),
            "category": "calls",
            "sources": [{"unit": "rift://source/project/Entry.java", "path": "Entry.java", "filename": "Entry.java"}]
        })
        .to_string(),
    )
    .expect("manifest");

    let mut syntax = command();
    syntax.arg("static").arg(&manifest).arg(&cache);
    let syntax = run(syntax).await;
    assert!(syntax.status.success(), "{:?}", syntax.stderr);
    let syntax: Value = serde_json::from_slice(&syntax.stdout).expect("syntax batch");
    assert_eq!(syntax["batch"]["observations"][0]["evidence"], "syntax");

    let mut kotlin = command();
    kotlin.args(["lsp", "kotlin"]).arg(&manifest).arg(&cache);
    let kotlin = run(kotlin).await;
    assert!(kotlin.status.success(), "{:?}", kotlin.stderr);
    let kotlin: Value = serde_json::from_slice(&kotlin.stdout).expect("LSP batch");
    assert_eq!(kotlin["batch"]["coverage"], "unsupported");
    assert_ne!(kotlin["context"], syntax["context"]);

    let mut java = command();
    java.args(["lsp", "java"])
        .arg(&manifest)
        .arg(&cache)
        .args(["--", "rift-unavailable-fixture-language-server"]);
    let java = run(java).await;
    assert_eq!(java.status.code(), Some(1));
    assert!(java.stdout.is_empty());

    std::fs::write(
        &source,
        "class Entry { int another() { return helper(); } }",
    )
    .expect("new source");
    let mut rerun = command();
    rerun.args(["lsp", "kotlin"]).arg(&manifest).arg(&cache);
    let rerun = run(rerun).await;
    assert!(rerun.status.success(), "{:?}", rerun.stderr);
    let rerun: Value = serde_json::from_slice(&rerun.stdout).expect("changed LSP batch");
    assert_ne!(rerun["context"], kotlin["context"]);
}
