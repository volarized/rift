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

/// Mirrors `steer::HOOK_STDIN_BYTES_MAX`; not importable across the
/// integration-test crate boundary.
const HOOK_STDIN_BYTES_MAX: usize = 1_048_576;

fn run_steer(root: &Path, stdin: &str, env: &[(&str, &str)]) -> TestResult<Output> {
    let mut command = Command::new(
        std::env::var_os("CARGO_BIN_EXE_rift")
            .ok_or("test runner must provide CARGO_BIN_EXE_rift")?,
    );
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
    tool_payload(
        tool_name,
        &serde_json::json!({"pattern": pattern}),
        session_id,
        cwd,
    )
}

fn tool_payload(
    tool_name: &str,
    tool_input: &serde_json::Value,
    session_id: &str,
    cwd: &Path,
) -> String {
    serde_json::json!({
        "session_id": session_id,
        "transcript_path": "/tmp/transcript.jsonl",
        "cwd": cwd.to_string_lossy(),
        "permission_mode": "default",
        "hook_event_name": "PreToolUse",
        "tool_name": tool_name,
        "tool_input": tool_input,
    })
    .to_string()
}

/// The `search` arguments a deny reason suggests: the one JSON object
/// following `tool: `.
fn suggested_call(output: &Output) -> TestResult<serde_json::Value> {
    let decision = decision(output)?;
    assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = decision["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("a deny carries a reason")?;
    let call = reason
        .split_once("tool: ")
        .and_then(|(_, rest)| {
            serde_json::Deserializer::from_str(rest)
                .into_iter::<serde_json::Value>()
                .next()
        })
        .ok_or_else(|| format!("the reason names no call: {reason}"))?;
    Ok(call?)
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
fn a_non_qualifying_tool_with_valid_json_answers_allow() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;
    let payload = hook_payload("Write", "TODO", "session-epsilon", root);

    let output = run_steer(root, &payload, &[])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        decision(&output)?["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    assert!(!root.join(".rift").join("steer").exists());
    Ok(())
}

#[test]
fn stdin_over_the_byte_bound_answers_allow_with_exit_zero() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;
    let oversized = "a".repeat(HOOK_STDIN_BYTES_MAX + 1);

    let output = run_steer(root, &oversized, &[])?;
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

#[test]
fn a_grep_pattern_holding_a_quote_and_a_backslash_suggests_valid_json() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;
    let pattern = r#"say\("hi"\)"#;
    let input = serde_json::json!({"pattern": pattern, "output_mode": "content", "-n": true});
    let payload = tool_payload("Grep", &input, "session-quote", root);

    let output = run_steer(root, &payload, &[])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        suggested_call(&output)?,
        serde_json::json!({"pattern": pattern})
    );
    Ok(())
}

#[test]
fn a_grep_call_search_has_no_form_for_answers_allow_and_creates_no_marker() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;
    let input = serde_json::json!({"pattern": "TODO", "output_mode": "content", "-C": 3});
    let payload = tool_payload("Grep", &input, "session-context", root);

    let output = run_steer(root, &payload, &[])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        decision(&output)?["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    assert!(!root.join(".rift").join("steer").exists());
    Ok(())
}

#[test]
fn a_bash_grep_from_a_subdirectory_denies_with_its_search_call() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;
    let source = root.join("src");
    fs::create_dir(&source)?;
    let input = serde_json::json!({"command": r#"grep -rn "cfg(test)" . 2>/dev/null"#});
    let payload = tool_payload("Bash", &input, "session-bash", &source);

    let output = run_steer(root, &payload, &[])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        suggested_call(&output)?,
        serde_json::json!({"pattern": r"cfg\(test\)", "paths": {"include": ["src/**"]}})
    );
    assert!(
        root.join(".rift")
            .join("steer")
            .join("session-bash")
            .exists()
    );
    Ok(())
}

#[test]
fn a_glob_call_from_a_subdirectory_denies_with_the_listing_call() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;
    let source = root.join("src");
    fs::create_dir(&source)?;
    let input = serde_json::json!({"pattern": "*.rs"});
    let payload = tool_payload("Glob", &input, "session-glob", &source);

    let output = run_steer(root, &payload, &[])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        suggested_call(&output)?,
        serde_json::json!({
            "pattern": r"\A",
            "paths": {"include": ["src/**/*.rs"]},
            "target": "file"
        })
    );
    Ok(())
}

#[test]
fn a_bash_pipeline_or_other_command_answers_allow_and_creates_no_marker() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    indexed_workspace(root)?;
    for command in [
        r#"grep -rn "cfg(test)" . | head -20"#,
        "cargo test",
        "grep -rn x missing-directory",
    ] {
        let input = serde_json::json!({"command": command});
        let payload = tool_payload("Bash", &input, "session-pipeline", root);
        let output = run_steer(root, &payload, &[])?;
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            decision(&output)?["hookSpecificOutput"]["permissionDecision"],
            "allow",
            "{command}"
        );
    }
    assert!(!root.join(".rift").join("steer").exists());
    Ok(())
}
