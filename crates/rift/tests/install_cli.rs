//! Real-binary contract of `rift install claude`: writes the generated
//! Claude Code skill, reruns idempotently, and `--remove` deletes it. The
//! `--user` scope writes under an overridden `HOME` and never touches the
//! workspace directory the command runs in.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn rift(root: &Path, arguments: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_rift"))
        .args(arguments)
        .current_dir(root)
        .output()?)
}

fn require_success(output: &Output, what: &str) -> TestResult {
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{what} must succeed: status {:?}, stdout {:?}, stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
    .into())
}

#[test]
fn install_writes_the_skill_and_reruns_idempotently() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();

    let installed = rift(root, &["install", "claude"])?;
    require_success(&installed, "install claude")?;
    let stdout = String::from_utf8(installed.stdout)?;
    assert!(
        stdout.contains("wrote the rift Claude Code skill"),
        "{stdout:?}"
    );

    let skill_root = root.join(".claude").join("skills").join("rift");
    let skill_md = fs::read_to_string(skill_root.join("SKILL.md"))?;
    assert!(skill_md.starts_with("---\nname: rift\n"), "{skill_md:?}");
    let tools_md = fs::read_to_string(skill_root.join("references").join("tools.md"))?;
    assert!(tools_md.starts_with("# Rift MCP tools"), "{tools_md:?}");

    let rerun = rift(root, &["install", "claude"])?;
    require_success(&rerun, "repeated install claude")?;
    assert_eq!(
        fs::read_to_string(skill_root.join("SKILL.md"))?,
        skill_md,
        "a rerun must write byte-identical content"
    );
    assert_eq!(
        fs::read_to_string(skill_root.join("references").join("tools.md"))?,
        tools_md
    );
    Ok(())
}

#[test]
fn remove_deletes_the_generated_skill_directory() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    require_success(
        &rift(root, &["install", "claude"])?,
        "install before remove",
    )?;
    let skill_root = root.join(".claude").join("skills").join("rift");
    assert!(skill_root.exists());

    let removed = rift(root, &["install", "claude", "--remove"])?;
    require_success(&removed, "install claude --remove")?;
    assert!(
        String::from_utf8_lossy(&removed.stdout).contains("removed the rift Claude Code skill")
    );
    assert!(!skill_root.exists());

    let repeated = rift(root, &["install", "claude", "--remove"])?;
    require_success(&repeated, "repeated remove")?;
    Ok(())
}

#[test]
fn user_scope_writes_under_the_overridden_home_and_never_touches_the_workspace() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;

    let output = Command::new(env!("CARGO_BIN_EXE_rift"))
        .args(["install", "claude", "--user"])
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .output()?;
    require_success(&output, "install claude --user")?;

    let user_skill_root = home.path().join(".claude").join("skills").join("rift");
    assert!(user_skill_root.join("SKILL.md").exists());
    assert!(
        !workspace.path().join(".claude").exists(),
        "a user-scope install must never touch the workspace"
    );
    Ok(())
}

#[test]
fn install_creates_a_fresh_settings_json_with_the_steering_hook() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    require_success(&rift(root, &["install", "claude"])?, "install claude")?;

    let settings_path = root.join(".claude").join("settings.json");
    let settings: serde_json::Value = serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["matcher"],
        serde_json::json!("Grep|Glob")
    );
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        serde_json::json!("rift steer")
    );
    Ok(())
}

#[test]
fn install_preserves_unrelated_hooks_and_settings() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let claude_directory = root.join(".claude");
    fs::create_dir_all(&claude_directory)?;
    fs::write(
        claude_directory.join("settings.json"),
        serde_json::json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "echo hi"}],
                }],
            },
        })
        .to_string(),
    )?;

    require_success(&rift(root, &["install", "claude"])?, "install claude")?;
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(claude_directory.join("settings.json"))?)?;
    assert_eq!(settings["model"], serde_json::json!("opus"));
    let groups = settings["hooks"]["PreToolUse"].as_array().expect("array");
    assert_eq!(groups.len(), 2);
    assert!(
        groups
            .iter()
            .any(|group| group["matcher"] == serde_json::json!("Bash"))
    );
    assert!(
        groups
            .iter()
            .any(|group| group["hooks"][0]["command"] == serde_json::json!("rift steer"))
    );
    Ok(())
}

#[test]
fn install_rerun_leaves_settings_json_byte_identical() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    require_success(&rift(root, &["install", "claude"])?, "install claude")?;
    let settings_path = root.join(".claude").join("settings.json");
    let first = fs::read_to_string(&settings_path)?;

    require_success(
        &rift(root, &["install", "claude"])?,
        "repeated install claude",
    )?;
    let second = fs::read_to_string(&settings_path)?;
    assert_eq!(first, second, "a rerun must write byte-identical content");
    Ok(())
}

#[test]
fn remove_strips_the_steering_hook_and_keeps_unrelated_content() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    require_success(
        &rift(root, &["install", "claude"])?,
        "install before remove",
    )?;
    let settings_path = root.join(".claude").join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
    settings["model"] = serde_json::json!("opus");
    fs::write(&settings_path, settings.to_string())?;

    require_success(
        &rift(root, &["install", "claude", "--remove"])?,
        "install claude --remove",
    )?;
    let after: serde_json::Value = serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
    assert_eq!(after["model"], serde_json::json!("opus"));
    assert!(
        after.get("hooks").is_none(),
        "the hooks key must be dropped once empty"
    );
    Ok(())
}

#[test]
fn unparseable_settings_json_is_refused_without_being_overwritten() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let claude_directory = root.join(".claude");
    fs::create_dir_all(&claude_directory)?;
    let settings_path = claude_directory.join("settings.json");
    fs::write(&settings_path, b"{ not json")?;

    let output = rift(root, &["install", "claude"])?;
    assert!(
        !output.status.success(),
        "an unparseable settings.json must refuse"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("settings.json"), "{stderr}");
    assert_eq!(
        fs::read_to_string(&settings_path)?,
        "{ not json",
        "the file must never be overwritten"
    );
    assert!(
        !claude_directory.join("skills").exists(),
        "a refused install must write nothing else"
    );
    Ok(())
}
