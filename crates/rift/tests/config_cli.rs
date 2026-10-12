//! Real-binary checks of telemetry configuration writes.

mod test_case;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn run(root: &Path, home: &Path, arguments: &[&str]) -> std::io::Result<Output> {
    let binary = std::env::var_os("CARGO_BIN_EXE_rift").expect("test runner provides rift binary");
    let mut command = Command::new(binary);
    test_case::with_test_case_name(&mut command);
    command
        .args(arguments)
        .current_dir(root)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
}

#[test]
fn test_config_cli_project_and_global_preserve_content_without_reporting() -> TestResult {
    let root = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let path = root.path().join("rift.toml");
    let original = "# keep\n[source]\nrespect_gitignore = false # choice\n";
    fs::write(&path, original)?;
    let output = run(
        root.path(),
        home.path(),
        &["config", "set", "--telemetry.disabled=true"],
    )?;
    assert!(output.status.success(), "{output:?}");
    let changed = fs::read_to_string(&path)?;
    assert!(changed.starts_with(original));
    assert!(changed.contains("disabled = true"));
    assert!(
        rift_server::load_configuration(root.path())?
            .telemetry
            .disabled
    );
    let output = run(
        root.path(),
        home.path(),
        &["config", "set", "--telemetry.disabled=false"],
    )?;
    assert!(output.status.success(), "{output:?}");
    assert!(fs::read_to_string(&path)?.contains("disabled = false"));

    let output = run(
        root.path(),
        home.path(),
        &["config", "set", "--user", "--telemetry.disabled=true"],
    )?;
    assert!(output.status.success(), "{output:?}");
    assert!(fs::read_to_string(home.path().join(".rift/config.toml"))?.contains("disabled = true"));
    assert_eq!(fs::read_dir(home.path().join(".rift"))?.count(), 1);
    Ok(())
}

#[test]
fn test_config_cli_refuses_invalid_value_document_and_unknown_key() -> TestResult {
    let root = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let path = root.path().join("rift.toml");
    fs::write(&path, "[broken")?;
    for option in [
        "--telemetry.disabled=true",
        "--telemetry.disabled=maybe",
        "--telemetry.unknown=true",
    ] {
        assert!(
            !run(root.path(), home.path(), &["config", "set", option])?
                .status
                .success()
        );
        assert_eq!(fs::read_to_string(&path)?, "[broken");
    }
    Ok(())
}

#[test]
fn test_config_cli_preserves_inline_comments_crlf_and_table_order() -> TestResult {
    let root = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let path = root.path().join("rift.toml");
    let original = "# header: café\r\nsource.respect_gitignore = false\r\ntelemetry = {\r\n  # setting comment\r\n  disabled = false,\r\n} # table comment\r\n";
    fs::write(&path, original)?;
    let output = run(
        root.path(),
        home.path(),
        &["config", "set", "--telemetry.disabled=true"],
    )?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fs::read_to_string(&path)?,
        original.replacen("disabled = false", "disabled = true", 1)
    );
    assert!(
        rift_server::load_configuration(root.path())?
            .telemetry
            .disabled
    );
    Ok(())
}
