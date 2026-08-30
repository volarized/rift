//! Integration coverage for the `rift-schema-export` CLI logic: argument
//! parsing, writing, `--check`, and the errors each path can produce.

use std::error::Error;
use std::fs;
use std::process::Command;

use rift_mcp::schema::{self, ExportError};
use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const REGENERATE_COMMAND: &str = "cargo run -p rift-mcp --bin rift-schema-export";

#[test]
fn run_without_arguments_writes_default_output_directory() -> TestResult {
    let directory = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_rift-schema-export"))
        .current_dir(directory.path())
        .output()?;
    assert!(output.status.success());

    let written = fs::read_to_string(directory.path().join("docs/public/mcp.json"))?;
    assert_eq!(written, schema::schema_document());
    let configuration = fs::read_to_string(directory.path().join("docs/public/rift.schema.json"))?;
    assert_eq!(configuration, schema::configuration_schema_document());

    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.starts_with("wrote "));
    assert!(stdout.contains("docs/public/mcp.json"));
    assert!(stdout.contains("docs/public/rift.schema.json"));
    Ok(())
}

#[test]
fn check_fails_when_configuration_schema_is_stale() -> TestResult {
    let directory = tempfile::tempdir()?;
    let write_request = schema::parse_arguments([directory.path().display().to_string()])?;
    schema::run(&write_request)?;
    fs::write(directory.path().join("public/rift.schema.json"), "{}")?;

    let check_request =
        schema::parse_arguments(["--check".to_owned(), directory.path().display().to_string()])?;
    let error = schema::run(&check_request).expect_err("stale configuration schema must fail");
    let ExportError::CheckMismatch { path } = error else {
        panic!("expected CheckMismatch, got {error:?}");
    };
    assert!(path.ends_with("rift.schema.json"));
    Ok(())
}

#[test]
fn configuration_schema_document_is_deterministic_and_validates_hooks() -> TestResult {
    let first = schema::configuration_schema_document();
    assert_eq!(first, schema::configuration_schema_document());
    let document: Value = serde_json::from_str(&first)?;
    assert_eq!(document["title"], "WorkspaceConfiguration");
    let hook = &document["$defs"]["CommandHook"];
    assert_eq!(hook["additionalProperties"], Value::Bool(false));
    let required: Vec<&str> = hook["required"]
        .as_array()
        .ok_or("hook required must be an array")?
        .iter()
        .map(|field| {
            field
                .as_str()
                .ok_or("hook required entries must be strings")
        })
        .collect::<Result<_, _>>()?;
    assert_eq!(
        required,
        ["id", "kind", "command", "determinism"],
        "only hook identity, kind, command, and determinism are required"
    );
    assert_eq!(hook["properties"]["writes"]["default"], "none");
    assert_eq!(hook["properties"]["failure_severity"]["default"], "error");
    Ok(())
}

#[test]
fn run_with_unknown_flag_prints_error_and_fails() -> TestResult {
    let directory = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_rift-schema-export"))
        .arg("--bogus")
        .current_dir(directory.path())
        .output()?;
    assert!(!output.status.success());

    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.starts_with("rift-schema-export: "));
    assert!(stderr.contains("--bogus"));
    Ok(())
}

#[test]
fn run_with_explicit_directory_writes_document_there() -> TestResult {
    let directory = tempfile::tempdir()?;
    let request = schema::parse_arguments([directory.path().display().to_string()])?;
    schema::run(&request)?;

    let written = fs::read_to_string(directory.path().join("public/mcp.json"))?;
    assert_eq!(written, schema::schema_document());
    Ok(())
}

#[test]
fn check_succeeds_when_document_matches_served_surface() -> TestResult {
    let directory = tempfile::tempdir()?;
    let write_request = schema::parse_arguments([directory.path().display().to_string()])?;
    schema::run(&write_request)?;

    let check_request =
        schema::parse_arguments(["--check".to_owned(), directory.path().display().to_string()])?;
    schema::run(&check_request)?;
    Ok(())
}

#[test]
fn check_fails_when_document_is_stale() -> TestResult {
    let directory = tempfile::tempdir()?;
    let write_request = schema::parse_arguments([directory.path().display().to_string()])?;
    schema::run(&write_request)?;
    fs::write(directory.path().join("public/mcp.json"), "{}")?;

    let check_request =
        schema::parse_arguments(["--check".to_owned(), directory.path().display().to_string()])?;
    let error = schema::run(&check_request).expect_err("stale document must fail check");
    assert!(matches!(error, ExportError::CheckMismatch { .. }));
    assert!(std::error::Error::source(&error).is_none());
    Ok(())
}

#[test]
fn check_fails_when_document_is_missing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let check_request =
        schema::parse_arguments(["--check".to_owned(), directory.path().display().to_string()])?;
    let error = schema::run(&check_request).expect_err("missing document must fail check");
    assert!(matches!(error, ExportError::CheckUnreadable { .. }));
    assert!(std::error::Error::source(&error).is_some());
    Ok(())
}

#[test]
fn parse_arguments_rejects_unknown_flag() {
    let error =
        schema::parse_arguments(["--bogus".to_owned()]).expect_err("unknown flag must fail");
    let message = error.to_string();
    assert_eq!(error.descriptor().code(), "invalid_request");
    let ExportError::UnknownFlag { argument } = error else {
        panic!("expected UnknownFlag, got {error:?}");
    };
    assert_eq!(argument, "--bogus");
    assert!(message.contains("--bogus"));
    assert!(message.contains("usage: rift-schema-export"));
}

#[test]
fn descriptor_codes_match_registry_per_variant() {
    let extra = schema::parse_arguments(["first".to_owned(), "second".to_owned()])
        .expect_err("second directory must fail");
    assert_eq!(extra.descriptor().code(), "invalid_request");
}

#[test]
fn check_unreadable_descriptor_is_storage_failure() -> TestResult {
    let directory = tempfile::tempdir()?;
    let check_request =
        schema::parse_arguments(["--check".to_owned(), directory.path().display().to_string()])?;
    let error = schema::run(&check_request).expect_err("missing document must fail check");
    assert_eq!(error.descriptor().code(), "storage_failure");
    Ok(())
}

#[test]
fn write_failed_descriptor_is_storage_failure() -> TestResult {
    let directory = tempfile::tempdir()?;
    let blocked = directory.path().join("blocked");
    fs::write(&blocked, "not a directory")?;
    let request = schema::parse_arguments([blocked.join("nested").display().to_string()])?;
    let error = schema::run(&request).expect_err("writing under a file must fail");
    assert_eq!(error.descriptor().code(), "storage_failure");
    Ok(())
}

#[test]
fn check_mismatch_descriptor_is_artifact_stale() -> TestResult {
    let directory = tempfile::tempdir()?;
    let write_request = schema::parse_arguments([directory.path().display().to_string()])?;
    schema::run(&write_request)?;
    fs::write(directory.path().join("public/mcp.json"), "{}")?;

    let check_request =
        schema::parse_arguments(["--check".to_owned(), directory.path().display().to_string()])?;
    let error = schema::run(&check_request).expect_err("stale document must fail check");
    assert_eq!(error.descriptor().code(), "artifact_stale");
    Ok(())
}

#[test]
fn parse_arguments_rejects_second_output_directory() {
    let error = schema::parse_arguments(["first".to_owned(), "second".to_owned()])
        .expect_err("second directory must fail");
    let message = error.to_string();
    let ExportError::ExtraArgument { argument } = error else {
        panic!("expected ExtraArgument, got {error:?}");
    };
    assert_eq!(argument, "second");
    assert!(message.contains("second"));
    assert!(message.contains("usage: rift-schema-export"));
}

#[test]
fn check_error_messages_name_path_and_regenerate_command() -> TestResult {
    let directory = tempfile::tempdir()?;
    let document_path = directory
        .path()
        .join("public/mcp.json")
        .display()
        .to_string();
    let check_request =
        schema::parse_arguments(["--check".to_owned(), directory.path().display().to_string()])?;

    let missing = schema::run(&check_request).expect_err("missing document must fail check");
    assert!(missing.to_string().contains(&document_path));
    assert!(missing.to_string().contains(REGENERATE_COMMAND));

    let write_request = schema::parse_arguments([directory.path().display().to_string()])?;
    schema::run(&write_request)?;
    fs::write(directory.path().join("public/mcp.json"), "{}")?;
    let mismatch = schema::run(&check_request).expect_err("stale document must fail check");
    assert!(mismatch.to_string().contains(&document_path));
    assert!(mismatch.to_string().contains(REGENERATE_COMMAND));
    Ok(())
}

#[test]
fn write_failed_names_path_and_usage() -> TestResult {
    let directory = tempfile::tempdir()?;
    let blocked = directory.path().join("blocked");
    fs::write(&blocked, "not a directory")?;
    let request = schema::parse_arguments([blocked.join("nested").display().to_string()])?;

    let error = schema::run(&request).expect_err("writing under a file must fail");
    assert!(matches!(error, ExportError::WriteFailed { .. }));
    assert!(std::error::Error::source(&error).is_some());
    assert!(error.to_string().contains("usage: rift-schema-export"));
    Ok(())
}

#[test]
fn write_failed_when_document_path_is_a_directory() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::create_dir_all(directory.path().join("public/mcp.json"))?;
    let request = schema::parse_arguments([directory.path().display().to_string()])?;

    let error = schema::run(&request).expect_err("writing over a directory must fail");
    assert!(matches!(error, ExportError::WriteFailed { .. }));
    assert!(std::error::Error::source(&error).is_some());
    Ok(())
}

#[test]
fn schema_document_is_deterministic_and_sorted_by_name() -> TestResult {
    let first = schema::schema_document();
    let second = schema::schema_document();
    assert_eq!(
        first, second,
        "schema_document must be byte-identical across calls"
    );

    let document: Value = serde_json::from_str(&first)?;
    let tools = document["tools"]
        .as_array()
        .ok_or("tools must be an array")?;
    assert!(!tools.is_empty());

    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().ok_or("tool name must be a string"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "tools must be sorted by name");

    for tool in tools {
        let name = tool["name"].as_str().ok_or("tool name must be a string")?;
        let input_schema = &tool["input_schema"];
        let since = input_schema["rift:since"]
            .as_str()
            .ok_or("every tool input model must declare rift:since")?;
        assert!(
            since.starts_with('v'),
            "tool version must use release spelling: name={name}, since={since}"
        );
        let example = input_schema["examples"]
            .as_array()
            .and_then(|examples| examples.first())
            .ok_or("every tool input model must declare an example")?;
        let validator = jsonschema::validator_for(input_schema)?;
        let failures: Vec<String> = validator
            .iter_errors(example)
            .map(|failure| failure.to_string())
            .collect();
        assert!(
            failures.is_empty(),
            "tool input example must validate: name={name}, failures={failures:#?}"
        );
    }
    Ok(())
}
