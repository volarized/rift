//! Real-binary contract of `rift steer`: a hook payload piped on stdin
//! answers a Claude Code `PreToolUse` decision on stdout, exit code 0 in
//! every case. The kernel's decision logic is unit-tested in
//! `crates/rift/src/steer.rs`; this proves the process-level wiring: reading
//! stdin, probing the real filesystem, and writing the marker.

use std::error::Error;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn run_steer(root: &Path, stdin: &str, env: &[(&str, &str)]) -> TestResult<Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rift"));
    command
        .arg("steer")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Inherits the rest of the ambient environment (PATH, dyld on macOS); only
    // RIFT_STEER is scrubbed so an operator's own exported kill switch cannot
    // flip a test's deny/allow expectation out from under its explicit `env` pair.
    command.env_remove("RIFT_STEER");
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .expect("stdin must be piped")
        .write_all(stdin.as_bytes())?;
    Ok(child.wait_with_output()?)
}

fn hook_payload(tool_name: &str, pattern: &str, session_id: &str, cwd: &Path) -> String {
    serde_json::json!({
        "session_id": session_id,
        "transcript_path": "/tmp/transcript.jsonl",
        "cwd": cwd.to_string_lossy(),
        "permission_mode": "default",
        "hook_event_name": "PreToolUse",
        "tool_name": tool_name,
        "tool_input": {"pattern": pattern},
    })
    .to_string()
}

fn indexed_workspace(root: &Path) -> TestResult {
    fs::create_dir_all(root.join(".rift"))?;
    fs::write(root.join(".rift").join("db"), b"")?;
    fs::create_dir(root.join(".git"))?;
    Ok(())
}

fn decision(output: &Output) -> TestResult<serde_json::Value> {
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn a_first_qualifying_grep_call_denies_and_creates_a_marker() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;

    let payload = hook_payload("Grep", "TODO", "session-alpha", root);
    let output = run_steer(root, &payload, &[])?;
    assert_eq!(output.status.code(), Some(0));
    let decision = decision(&output)?;
    assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = decision["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .expect("a deny carries a reason");
    assert!(reason.contains("Grep"), "{reason}");
    assert!(reason.contains("TODO"), "{reason}");
    assert!(
        root.join(".rift")
            .join("steer")
            .join("session-alpha")
            .exists()
    );
    Ok(())
}

#[test]
fn a_second_call_in_the_same_session_answers_allow() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;
    let payload = hook_payload("Grep", "TODO", "session-beta", root);

    let first = run_steer(root, &payload, &[])?;
    assert_eq!(
        decision(&first)?["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );

    let second = run_steer(root, &payload, &[])?;
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(
        decision(&second)?["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    Ok(())
}

#[test]
fn rift_steer_zero_disables_the_hook_and_creates_no_marker() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;
    let payload = hook_payload("Grep", "TODO", "session-gamma", root);

    let output = run_steer(root, &payload, &[("RIFT_STEER", "0")])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        decision(&output)?["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    assert!(!root.join(".rift").join("steer").exists());
    Ok(())
}

#[test]
fn a_workspace_without_an_index_answers_allow() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::create_dir(root.join(".git"))?;
    let payload = hook_payload("Grep", "TODO", "session-delta", root);

    let output = run_steer(root, &payload, &[])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        decision(&output)?["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    Ok(())
}

#[test]
fn malformed_stdin_answers_allow_with_exit_zero() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;

    let output = run_steer(root, "not json", &[])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        decision(&output)?["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.is_empty(), "steer never fails: {stderr}");
    Ok(())
}
